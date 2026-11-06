//! ACP (Agent Client Protocol) adapter, for `cursor-agent acp`, `kiro-cli acp`, `qodercli --acp`, `codebuddy --acp`.
//! Protocol: JSON-RPC 2.0 over stdio; session/update notifications push content, session/request_permission asks for approval.

use super::{AdapterCmd, JsonProc, Prompt, StartSpec};
use crate::hub::Emitter;
use crate::util::now_ms;
use yi_proto::*;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Pending {
    Init,
    Auth,
    Session,
    /// session/load: switch to session/new when the tool can't find this session
    Load,
    Prompt,
    SetModel,
    SetConfig,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Seg {
    Agent,
    Thought,
    User,
}

#[derive(Default, Clone)]
struct ToolState {
    name: String,
    title: String,
    input: Value,
    diffs: Vec<FileDiff>,
    done: bool,
}

#[derive(Default)]
struct State {
    /// The computer use notice still has to go out with the next prompt (once per launch)
    desktop_notice: bool,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    session_id: Option<String>,
    prompting: bool,
    queue: Vec<Prompt>,
    /// initialize declared support for image input
    image_ok: bool,
    /// Model switch requested before the session was created
    want_model: Option<String>,
    /// Model being switched to; recorded on the session once it succeeds
    switching: Option<String>,
    /// Cursor's parameterized picker is negotiated per process. Config changes run in order before prompts.
    parameterized: bool,
    config_options: Vec<ModelConfigOption>,
    config_model: Option<String>,
    config_queue: Vec<(String, String)>,
    seg: Option<(Seg, String, String)>,
    seg_n: u64,
    tools: HashMap<String, ToolState>,
    perms: HashMap<String, Value>,
    /// Subagent (Agent / Task) tool calls still running in the foreground. Qoder's ACP bridge forwards every subagent's
    /// streamed text, thinking and tool calls as the parent session's own updates, with nothing to tell them apart, so
    /// parallel subagents came out as one reply cut mid-word by "thinking" blocks. The parent is blocked on these calls,
    /// so while any runs, chunks and new tool calls belong to subagents: they are dropped like Claude's subagent
    /// messages (`parent_tool_use_id`), and the subagent's report arrives as the Agent call's result
    subagents: HashSet<String>,
    /// Tool calls started by subagents, hidden along with their updates
    hidden: HashSet<String>,
    /// During session/load replay
    loading: bool,
    record_replay: bool,
    /// During replay, skip this many leading user messages, and how far the replay has got
    replay_skip: usize,
    replay_users: usize,
    plan_mode: Option<String>,
    /// The tool's mode when the session was created (not the plan one); switched back to when leaving read-only planning
    base_mode: Option<String>,
    /// Current permission mode; can change during the session
    perm: String,
    turn_credits: f64,
    /// Cursor's cursor/ask_question requests: request id -> (JSON-RPC id)
    asks: HashMap<String, Value>,
    /// Auth methods returned by initialize. We don't call authenticate up front (cursor_login opens a browser,
    /// and with CURSOR_API_KEY set it isn't needed at all); only when session creation is refused do we log in with it and retry once.
    auth_method: Option<String>,
    auth_tried: bool,
    /// This run continues an existing session (session/load was sent) rather than a new one; reset to false when load fails and we fall back to a new session
    resumed: bool,
    /// Kiro compacts in the background after its /compact turn has ended, and a prompt sent meanwhile never gets
    /// an answer. Between `_kiro.dev/compaction/status` started and completed, messages wait in `queue`
    compacting: Option<i64>,
    /// The last prompt we sent was /compact, so the next compaction is manual
    compact_manual: bool,
}

impl State {
    fn req(&mut self, kind: Pending, method: &str, params: Value) -> Value {
        self.next_id += 1;
        self.pending.insert(self.next_id, kind);
        json!({"jsonrpc":"2.0","id": self.next_id, "method": method, "params": params})
    }

    /// A turn ended: a subagent call the tool never reported as finished mustn't swallow the next turn
    fn end_subagents(&mut self) {
        self.subagents.clear();
        self.hidden.clear();
    }

    /// When replaying history that doesn't need recording, drop content events
    fn muted(&self) -> bool {
        self.loading && (!self.record_replay || (self.replay_skip > 0 && self.replay_users <= self.replay_skip))
    }
}

/// Arguments for entering ACP mode: a subcommand for Cursor and Kiro, a flag for Qoder and CodeBuddy
pub fn acp_arg(tool: ToolKind) -> &'static str {
    match tool {
        ToolKind::Qoder | ToolKind::Codebuddy => "--acp",
        _ => "acp",
    }
}

pub fn build_args(spec: &StartSpec) -> Vec<String> {
    let mut a = vec![acp_arg(spec.tool).to_string()];
    if spec.tool == ToolKind::Kiro {
        if spec.permission_mode == "bypass" {
            a.push("--trust-all-tools".into());
        }
        if let Some(m) = spec.model.as_ref().filter(|m| !m.is_empty()) {
            a.push("--model".into());
            a.push(m.clone());
        }
    }
    a.extend(spec.extra_args.iter().cloned());
    if spec.tool == ToolKind::Kiro && spec.permission_mode != "bypass" && spec.mcp_servers.iter().any(|server| server["name"] == "yi_computer_use") {
        // Trust only the user-selected gateway server; its live window grant still gates every native call.
        let rules = crate::computer_use::tool_names().map(|name| format!("@yi_computer_use/{name}")).collect::<Vec<_>>().join(",");
        if let Some(index) = a.iter().rposition(|arg| arg == "--trust-tools" || arg.starts_with("--trust-tools=")) {
            if a[index].contains('=') { a[index].push(','); a[index].push_str(&rules); }
            else if let Some(value) = a.get_mut(index + 1) { value.push(','); value.push_str(&rules); }
        } else { a.extend(["--trust-tools".into(), rules]); }
    }
    a
}

pub async fn run(spec: StartSpec, em: Emitter, mut rx: mpsc::UnboundedReceiver<AdapterCmd>) {
    let args = build_args(&spec);
    let mut p = match JsonProc::spawn(&spec.program, &args, &spec.cwd) {
        Ok(p) => p,
        Err(e) => return em.exited(Some(format!("启动 {} 失败: {e}", spec.tool.as_str()))),
    };
    let mut st = State {
        perm: spec.permission_mode.clone(),
        desktop_notice: spec.mcp_servers.iter().any(|server| server["name"] == crate::mcp::DESKTOP_SERVER),
        ..Default::default()
    };
    let init = st.req(
        Pending::Init,
        "initialize",
        json!({"protocolVersion":1,
               "clientCapabilities":client_capabilities(spec.tool),
               "clientInfo":{"name":"yi-console","version":DAEMON_VERSION}}),
    );
    let _ = p.send(&init).await;

    let mut killed = false;
    let mut fatal: Option<String> = None;
    loop {
        tokio::select! {
            line = p.stdout.next_line() => {
                let Ok(Some(line)) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                match handle(&mut st, &spec, &v, &em) {
                    Ok(outs) => for out in outs { let _ = p.send(&out).await; },
                    Err(msg) => { fatal = Some(msg); killed = true; break; }
                }
                if let Some(m) = next_config(&mut st) { let _ = p.send(&m).await; }
                if st.session_id.is_some() && !config_pending(&st) && !st.prompting && !st.loading && st.compacting.is_none() && !st.queue.is_empty() {
                    let next = st.queue.remove(0);
                    let m = prompt(&mut st, &em, &next);
                    let _ = p.send(&m).await;
                }
            }
            cmd = rx.recv() => match cmd {
                Some(AdapterCmd::Send(next)) => {
                    em.status(SessionStatus::Running, None);
                    if st.session_id.is_some() && !config_pending(&st) && !st.prompting && !st.loading && st.compacting.is_none() {
                        let m = prompt(&mut st, &em, &next);
                        let _ = p.send(&m).await;
                    } else {
                        st.queue.push(next);
                    }
                }
                Some(AdapterCmd::SetModel(model)) => {
                    if st.session_id.is_some() {
                        if st.parameterized {
                            st.config_queue.push(("model".into(), model));
                            if let Some(m) = next_config(&mut st) { let _ = p.send(&m).await; }
                        } else {
                            let m = set_model(&mut st, &model);
                            let _ = p.send(&m).await;
                        }
                    } else {
                        st.want_model = Some(model);
                    }
                }
                Some(AdapterCmd::SetModelConfig { id, value }) => {
                    st.config_queue.push((id, value));
                    if let Some(m) = next_config(&mut st) { let _ = p.send(&m).await; }
                }
                Some(AdapterCmd::SetPermissionMode(mode)) => {
                    let was_plan = st.perm == "plan";
                    st.perm = mode;
                    if let Some(m) = apply_mode(&mut st, was_plan) {
                        let _ = p.send(&m).await;
                    }
                }
                Some(AdapterCmd::Interrupt) => {
                    // Messages held during a compaction have no turn to cancel: drop them and end the wait here
                    if st.compacting.is_some() && !st.prompting && !st.queue.is_empty() {
                        st.queue.clear();
                        em.event(EventBody::TurnEnd { stop_reason: Some("cancelled".into()), model: None });
                    }
                    if let Some(sid) = st.session_id.clone() {
                        for (rid, rpc_id) in st.perms.drain().chain(st.asks.drain()) {
                            let _ = p.send(&json!({"jsonrpc":"2.0","id":rpc_id,
                                "result":{"outcome":{"outcome":"cancelled"}}})).await;
                            em.event(EventBody::PermissionResolved { request_id: rid, option_id: "cancelled".into(), answers: vec![] });
                        }
                        let _ = p.send(&json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":sid}})).await;
                    }
                }
                Some(AdapterCmd::Permission { request_id, option_id, answers }) => {
                    if let Some(rpc_id) = st.asks.remove(&request_id) {
                        let outcome = match option_id.as_str() {
                            QUESTION_ANSWER => json!({"outcome":"answered","answers":answers.iter()
                                .filter(|a| !a.selected.is_empty())
                                .map(|a| json!({"questionId":a.question_id,"selectedOptionIds":a.selected}))
                                .collect::<Vec<_>>()}),
                            "cancelled" => json!({"outcome":"cancelled"}),
                            _ => json!({"outcome":"skipped","reason":"User skipped questions"}),
                        };
                        let _ = p.send(&json!({"jsonrpc":"2.0","id":rpc_id,"result":{"outcome":outcome}})).await;
                        em.event(EventBody::PermissionResolved { request_id, option_id, answers });
                    } else if let Some(rpc_id) = st.perms.remove(&request_id) {
                        let outcome = if option_id == "cancelled" {
                            json!({"outcome":"cancelled"})
                        } else {
                            json!({"outcome":"selected","optionId":option_id})
                        };
                        let _ = p.send(&json!({"jsonrpc":"2.0","id":rpc_id,"result":{"outcome":outcome}})).await;
                        em.event(EventBody::PermissionResolved { request_id, option_id, answers: vec![] });
                    }
                }
                Some(AdapterCmd::Kill) | None => { killed = true; break; }
                Some(_) => {}
            }
        }
    }
    flush(&mut st, &em);
    let msg = p.finish(killed).await;
    em.exited(fatal.or(msg));
}

/// Cursor only exposes individual model parameters when this extension is declared.
pub fn client_capabilities(tool: ToolKind) -> Value {
    let mut caps = json!({"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false});
    if tool == ToolKind::Cursor { caps["_meta"] = json!({"parameterizedModelPicker":true}); }
    caps
}

fn config_pending(st: &State) -> bool {
    !st.config_queue.is_empty() || st.pending.values().any(|p| matches!(p, Pending::SetModel | Pending::SetConfig))
}

fn next_config(st: &mut State) -> Option<Value> {
    if st.loading || st.session_id.is_none() || !st.parameterized || st.pending.values().any(|p| matches!(p, Pending::SetModel | Pending::SetConfig)) {
        return None;
    }
    while !st.config_queue.is_empty() {
        let (id, value) = st.config_queue.remove(0);
        // Saved parameters may no longer exist after a model switch or tool upgrade.
        if id != "model" && !st.config_options.iter().any(|o| o.id == id && o.options.iter().any(|v| v.value == value)) { continue; }
        return Some(st.req(Pending::SetConfig, "session/set_config_option",
            json!({"sessionId":st.session_id,"configId":id,"value":value})));
    }
    None
}

fn apply_config_options(st: &mut State, r: &Value, em: &Emitter) {
    let Some(options) = r["configOptions"].as_array() else { return; };
    if let Some(id) = options.iter().find(|o| o["id"] == "model").and_then(|o| o["currentValue"].as_str()) { st.config_model = Some(id.into()); em.model(id); }
    st.config_options = options.iter().filter(|o| matches!(o["category"].as_str(), Some("model_config" | "thought_level")))
        .filter_map(|o| serde_json::from_value::<ModelConfigOption>(o.clone()).ok()).collect();
    em.model_config_options(st.config_options.clone());
}

fn prompt(st: &mut State, em: &Emitter, p: &Prompt) -> Value {
    st.prompting = true;
    st.compact_manual = p.text.trim().starts_with("/compact");
    st.turn_credits = 0.0;
    let sid = st.session_id.clone().unwrap_or_default();
    let mut blocks = vec![];
    if !p.images.is_empty() {
        if st.image_ok {
            for i in &p.images {
                blocks.push(json!({"type":"image","mimeType":i.mime_type,"data":i.data}));
            }
        } else {
            em.event(EventBody::Error {
                message: "这个工具不支持图片输入，图片没有发送".into(),
            });
        }
    }
    // Slash commands must stay the first block for the tool to recognize them; the notice waits for a real prompt
    if st.desktop_notice && !p.text.trim_start().starts_with('/') {
        st.desktop_notice = false;
        blocks.push(json!({"type":"text","text":crate::computer_use::acp_notice()}));
    }
    if !p.text.trim().is_empty() || blocks.is_empty() {
        blocks.push(json!({"type":"text","text":p.text}));
    }
    st.req(
        Pending::Prompt,
        "session/prompt",
        json!({"sessionId":sid,"prompt":blocks}),
    )
}

/// Of the permission modes, only read-only planning needs the tool's cooperation (ACP session mode); the rest we handle at approval time
fn apply_mode(st: &mut State, was_plan: bool) -> Option<Value> {
    let sid = st.session_id.clone()?;
    let target = if st.perm == "plan" {
        st.plan_mode.clone()?
    } else if was_plan {
        st.base_mode.clone()?
    } else {
        return None;
    };
    Some(st.req(Pending::Other, "session/set_mode", json!({"sessionId":sid,"modeId":target})))
}

fn set_model(st: &mut State, model: &str) -> Value {
    let sid = st.session_id.clone().unwrap_or_default();
    st.switching = Some(model.to_string());
    st.req(
        Pending::SetModel,
        "session/set_model",
        json!({"sessionId":sid,"modelId":model}),
    )
}

fn flush(st: &mut State, em: &Emitter) {
    let Some((seg, id, text)) = st.seg.take() else {
        return;
    };
    if st.loading && seg == Seg::User && !text.trim().is_empty() {
        st.replay_users += 1;
    }
    if text.trim().is_empty() || st.muted() {
        return;
    }
    em.event(match seg {
        Seg::Agent => EventBody::AssistantText { id, text },
        Seg::Thought => EventBody::Thinking { id, text },
        Seg::User => EventBody::UserMessage { text, images: vec![] },
    });
}

fn chunk(st: &mut State, em: &Emitter, seg: Seg, text: &str) {
    if seg != Seg::User && !st.loading && !st.subagents.is_empty() {
        return;
    }
    let same = matches!(&st.seg, Some((s, _, _)) if *s == seg);
    if !same {
        flush(st, em);
        st.seg_n += 1;
        st.seg = Some((seg, format!("{}-{}", em.session_id(), st.seg_n), String::new()));
    }
    if let Some((_, id, buf)) = st.seg.as_mut() {
        buf.push_str(text);
        if !st.loading {
            match seg {
                Seg::Agent => em.event(EventBody::AssistantDelta {
                    id: id.clone(),
                    delta: text.to_string(),
                }),
                Seg::Thought => em.event(EventBody::ThinkingDelta {
                    id: id.clone(),
                    delta: text.to_string(),
                }),
                Seg::User => {}
            }
        }
    }
}

fn content_text(c: &Value) -> String {
    if let Some(t) = c["text"].as_str() {
        return t.to_string();
    }
    String::new()
}

/// When cursor-agent creates a file it stuffs the diff header into the body: oldText is "-- /dev/null",
/// and the first line of newText is "++ b/<path>". Strip these and treat it as a new file.
fn clean_diff_texts(old: Option<&str>, new: Option<&str>) -> (Option<String>, Option<String>) {
    let is_header = |l: &str| l.starts_with("++ b/") || l.starts_with("+++ b/") || l.starts_with("+++ /");
    let old = old.filter(|o| !matches!(o.trim(), "-- /dev/null" | "--- /dev/null"));
    let new = new.map(|n| match n.split_once('\n') {
        Some((first, rest)) if old.is_none() && is_header(first) => rest,
        None if old.is_none() && is_header(n) => "",
        _ => n,
    });
    (old.map(str::to_string), new.map(str::to_string))
}

fn handle(st: &mut State, spec: &StartSpec, v: &Value, em: &Emitter) -> Result<Vec<Value>, String> {
    let mut out = vec![];
    let method = v["method"].as_str();

    // Responses
    if method.is_none() {
        let Some(id) = v["id"].as_u64() else {
            return Ok(out);
        };
        let kind = st.pending.remove(&id).unwrap_or(Pending::Other);
        if let Some(err) = v.get("error") {
            let msg = err["data"]["message"]
                .as_str()
                .or(err["message"].as_str())
                .unwrap_or("ACP 错误")
                .to_string();
            // Kiro won't let two processes open a session at once; when it's taken, load only reports "Internal error", so check its lock file to explain
            if matches!(kind, Pending::Load) && spec.tool == ToolKind::Kiro {
                if let Some(pid) = spec.resume.as_deref().and_then(kiro_lock_holder) {
                    return Err(format!(
                        "这个会话正在另一个 Kiro 里打开（进程 {pid}），Kiro 不允许同时打开同一个会话。\
                         先退出那个 Kiro（比如关掉运行 kiro-cli 的终端窗口），再恢复会话"
                    ));
                }
            }
            return match kind {
                // If the process went away after the session was created but before anything was said, some tools (Cursor) never saved the session and load reports not found.
                // The old session had no content anyway, so just start a new one; don't treat it as not logged in
                Pending::Load if is_missing_session(&msg) => {
                    st.loading = false;
                    st.resumed = false;
                    out.push(new_session(st, spec));
                    Ok(out)
                }
                Pending::Session | Pending::Load if !st.auth_tried && st.auth_method.is_some() => {
                    st.auth_tried = true;
                    let mid = st.auth_method.clone().unwrap_or_default();
                    out.push(st.req(Pending::Auth, "authenticate", json!({"methodId": mid})));
                    Ok(out)
                }
                Pending::Auth | Pending::Session | Pending::Load | Pending::Init => Err(login_hint(spec, &msg)),
                Pending::Prompt => {
                    st.prompting = false;
                    st.end_subagents();
                    flush(st, em);
                    em.event(EventBody::Error { message: msg });
                    em.event(EventBody::TurnEnd {
                        stop_reason: Some("error".into()),
                        model: None,
                    });
                    Ok(out)
                }
                Pending::SetModel => {
                    st.switching = None;
                    em.event(EventBody::Error {
                        message: format!("切换模型失败: {msg}"),
                    });
                    Ok(out)
                }
                Pending::SetConfig => {
                    if let Some(model) = &st.config_model { em.model(model); }
                    em.model_config_options(st.config_options.clone());
                    em.event(EventBody::Error { message: format!("切换模型参数失败: {msg}") });
                    Ok(out)
                }
                Pending::Other => Ok(out),
            };
        }
        let r = &v["result"];
        match kind {
            Pending::Init => {
                if spec.mcp_servers.iter().any(|server| server["type"] == "http") && r["agentCapabilities"]["mcpCapabilities"]["http"] != true {
                    return Err("该工具未声明支持 HTTP MCP，请改用 stdio 或调整服务的工具范围".into());
                }
                st.image_ok = r["agentCapabilities"]["promptCapabilities"]["image"]
                    .as_bool()
                    .unwrap_or(false);
                st.auth_method = r["authMethods"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|m| m["id"].as_str())
                    .map(str::to_string);
                out.push(new_or_load(st, spec));
            }
            Pending::Auth => out.push(new_or_load(st, spec)),
            Pending::Session | Pending::Load => {
                if let Some(sid) = r["sessionId"].as_str() {
                    st.session_id = Some(sid.to_string());
                    em.external_id(sid);
                } else if let (true, Some(sid)) = (st.resumed, &spec.resume) {
                    st.session_id = Some(sid.clone());
                }
                flush(st, em);
                st.loading = false;
                if st.resumed {
                    let ctx = match spec.tool {
                        ToolKind::Cursor => st.session_id.as_deref().and_then(cursor_context),
                        ToolKind::Qoder => st.session_id.as_deref().and_then(qoder_context),
                        _ => None,
                    };
                    if let Some(p) = ctx {
                        em.context(p);
                    }
                }
                let models = parse_models(r);
                st.parameterized = spec.tool == ToolKind::Cursor && r["configOptions"].is_array();
                if st.parameterized { apply_config_options(st, r, em); }
                let current = r["models"]["currentModelId"].as_str().map(str::to_string);
                // If the model recorded on the session (picked at creation or switched later) differs from the tool's current one, switch to it
                let queued_config = std::mem::take(&mut st.config_queue);
                let want = st
                    .want_model
                    .take()
                    .or(spec.model.clone())
                    .map(|m| if st.parameterized { m.split('[').next().unwrap_or(&m).to_string() } else { m })
                    .filter(|m| !m.is_empty() && models.iter().any(|o| &o.id == m));
                match (want, &current) {
                    (Some(w), c) if c.as_deref() != Some(w.as_str()) => {
                        if st.parameterized { st.config_queue.insert(0, ("model".into(), w)); }
                        else { out.push(set_model(st, &w)); }
                    },
                    (_, Some(c)) => em.model(c),
                    _ => {}
                }
                if st.parameterized {
                    if let Some(params) = spec.model.as_deref().and_then(|m| m.split_once('[')).map(|(_, p)| p.trim_end_matches(']')) {
                        for param in params.split(',') {
                            if let Some((id, value)) = param.split_once('=') {
                                let id = if id.trim() == "reasoning_effort" { "reasoning" } else { id.trim() };
                                st.config_queue.push((id.into(), value.trim().into()));
                            }
                        }
                    }
                    // Restore after the model switch, before any queued user prompt.
                    for o in &spec.model_config_options { st.config_queue.push((o.id.clone(), o.current_value.clone())); }
                }
                st.config_queue.extend(queued_config);
                em.models(models);
                let modes = r["modes"]["availableModes"].as_array().cloned().unwrap_or_default();
                st.plan_mode = modes
                    .iter()
                    .filter_map(|m| m["id"].as_str())
                    .find(|id| id.contains("plan"))
                    .map(str::to_string);
                st.base_mode = r["modes"]["currentModeId"]
                    .as_str()
                    .filter(|m| !m.contains("plan"))
                    .map(str::to_string)
                    .or_else(|| {
                        modes
                            .iter()
                            .filter_map(|m| m["id"].as_str())
                            .find(|id| !id.contains("plan"))
                            .map(str::to_string)
                    });
                if st.perm == "plan" {
                    if let Some(m) = apply_mode(st, false) {
                        out.push(m);
                    }
                }
                if st.queue.is_empty() {
                    em.status(SessionStatus::Idle, None);
                }
            }
            Pending::Prompt => {
                st.prompting = false;
                st.end_subagents();
                flush(st, em);
                if let Some(msg) = meta_error(r) {
                    em.event(EventBody::Error { message: login_hint(spec, &msg) });
                }
                if st.turn_credits > 0.0 {
                    em.event(EventBody::Usage {
                        usage: Usage {
                            credits: st.turn_credits,
                            ..Default::default()
                        },
                    });
                    st.turn_credits = 0.0;
                }
                let ctx = match spec.tool {
                    ToolKind::Cursor => st.session_id.as_deref().and_then(cursor_context),
                    ToolKind::Qoder => st.session_id.as_deref().and_then(qoder_context),
                    _ => None,
                };
                if let Some(p) = ctx {
                    em.context(p);
                }
                em.event(EventBody::TurnEnd {
                    stop_reason: r["stopReason"].as_str().map(str::to_string),
                    model: None,
                });
            }
            Pending::SetModel => {
                if let Some(m) = st.switching.take() {
                    em.model(&m);
                }
            }
            Pending::SetConfig => apply_config_options(st, r, em),
            Pending::Other => {}
        }
        return Ok(out);
    }

    let method = method.unwrap_or("");
    let params = &v["params"];

    // agent -> client requests
    if !v["id"].is_null() {
        if method == "session/request_permission" {
            flush(st, em);
            let rid = v["id"].to_string();
            let tc = &params["toolCall"];
            let tc_id = tc["toolCallId"].as_str().unwrap_or("");
            let known = st.tools.get(tc_id).cloned().unwrap_or_default();
            let title = tc["title"]
                .as_str()
                .map(str::to_string)
                .unwrap_or(known.title.clone());
            let options: Vec<PermissionOption> = params["options"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|o| PermissionOption {
                    id: o["optionId"].as_str().unwrap_or("").to_string(),
                    label: o["name"].as_str().unwrap_or("").to_string(),
                    kind: o["kind"].as_str().unwrap_or("").to_string(),
                })
                .collect();
            // Allow everything, or accept-edits mode and this is a file edit
            let is_edit = tc["kind"] == "edit" || known.name == "edit" || !known.diffs.is_empty();
            if st.perm == "bypass" || (st.perm == "acceptEdits" && is_edit) {
                // Prefer "allow once": choosing "allow for this session" in Qoder also switches it into accept-edits mode
                let once = options.iter().find(|o| o.kind == "allow_once");
                if let Some(o) = once.or_else(|| options.iter().find(|o| o.kind.starts_with("allow"))) {
                    out.push(json!({"jsonrpc":"2.0","id":v["id"],
                        "result":{"outcome":{"outcome":"selected","optionId":o.id}}}));
                    return Ok(out);
                }
            }
            st.perms.insert(rid.clone(), v["id"].clone());
            em.event(EventBody::PermissionRequest {
                request_id: rid,
                tool_name: if known.name.is_empty() {
                    "tool".into()
                } else {
                    known.name
                },
                title,
                input: tc["rawInput"].clone(),
                options,
                questions: vec![],
            });
        } else if method == "cursor/ask_question" {
            // Cursor's question extension: {toolCallId, title, questions: [{id, prompt, options: [{id, label}], allowMultiple}]},
            // options only, no free text. When this method isn't supported, it falls back to one request_permission per question
            flush(st, em);
            let rid = v["id"].to_string();
            let questions: Vec<Question> = params["questions"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|q| Question {
                    id: q["id"].as_str().unwrap_or("").to_string(),
                    header: String::new(),
                    question: q["prompt"].as_str().unwrap_or("").to_string(),
                    options: q["options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|o| QuestionOption {
                            id: o["id"].as_str().unwrap_or("").to_string(),
                            label: o["label"].as_str().unwrap_or("").to_string(),
                            description: String::new(),
                        })
                        .collect(),
                    multi_select: q["allowMultiple"].as_bool().unwrap_or(false),
                    allow_other: false,
                    secret: false,
                })
                .collect();
            st.asks.insert(rid.clone(), v["id"].clone());
            em.event(EventBody::PermissionRequest {
                request_id: rid,
                tool_name: "ask_question".into(),
                title: params["title"].as_str().filter(|t| !t.is_empty()).unwrap_or("Cursor 有问题想问你").to_string(),
                input: params.clone(),
                options: super::question_options(),
                questions,
            });
        } else {
            out.push(json!({"jsonrpc":"2.0","id":v["id"],
                "error":{"code":-32601,"message":"not supported by yi-console"}}));
        }
        return Ok(out);
    }

    // Notifications
    match method {
        "session/update" => update(st, em, &params["update"]),
        "_kiro.dev/compaction/status" => compaction_status(st, em, &params["status"]["type"]),
        "_kiro.dev/metadata" => {
            if let Some(p) = params["contextUsagePercentage"].as_f64() {
                em.context(p);
            }
            for m in params["meteringUsage"].as_array().into_iter().flatten() {
                if m["unit"] == "credit" {
                    st.turn_credits += m["value"].as_f64().unwrap_or(0.0);
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

/// Kiro's `_kiro.dev/compaction/status`: status.type is started, then completed (or failed)
fn compaction_status(st: &mut State, em: &Emitter, kind: &Value) {
    let kind = kind.as_str().unwrap_or("");
    if kind == "started" {
        if st.compacting.is_none() {
            st.compacting = Some(now_ms());
            em.event(EventBody::Compact { phase: "start".into(), trigger: None, pre_tokens: None, post_tokens: None, duration_ms: None });
        }
        return;
    }
    // Any other status ends the compaction, so held messages are never stuck
    let Some(t0) = st.compacting.take() else { return };
    let ok = kind == "completed";
    em.event(EventBody::Compact {
        phase: if ok { "done" } else { "failed" }.into(),
        trigger: ok.then(|| if std::mem::take(&mut st.compact_manual) { "manual" } else { "auto" }.into()),
        pre_tokens: None,
        post_tokens: None,
        duration_ms: ok.then(|| (now_ms() - t0).max(0) as u64),
    });
}

fn new_or_load(st: &mut State, spec: &StartSpec) -> Value {
    let cwd = spec.cwd.display().to_string();
    match &spec.resume {
        Some(sid) => {
            st.loading = true;
            st.record_replay = spec.replay;
            st.replay_skip = spec.replay_skip;
            st.replay_users = 0;
            st.resumed = true;
            st.req(
                Pending::Load,
                "session/load",
                json!({"sessionId":sid,"cwd":cwd,"mcpServers":spec.mcp_servers}),
            )
        }
        None => new_session(st, spec),
    }
}

fn new_session(st: &mut State, spec: &StartSpec) -> Value {
    let cwd = spec.cwd.display().to_string();
    st.req(Pending::Session, "session/new", json!({"cwd":cwd,"mcpServers":spec.mcp_servers}))
}

/// Kiro writes ~/.kiro/sessions/cli/<id>.lock ({"pid":..,"started_at":..}) when it opens a session and deletes it on exit.
/// Returns the pid of the Kiro process holding this session's lock if it's still alive
fn kiro_lock_holder(sid: &str) -> Option<u32> {
    if sid.is_empty() || sid.contains(['/', '\\', '.']) {
        return None;
    }
    let path = crate::paths::home().join(".kiro").join("sessions").join("cli").join(format!("{sid}.lock"));
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let pid = u32::try_from(v["pid"].as_u64()?).ok()?;
    (pid != std::process::id() && kiro_alive(pid)).then_some(pid)
}

/// The process still exists and is Kiro (the pid may have been reused by another program)
#[cfg(windows)]
fn kiro_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let running = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let name = if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0 {
            String::from_utf16_lossy(&buf[..len as usize]).to_lowercase()
        } else {
            String::new()
        };
        CloseHandle(h);
        running && name.contains("kiro")
    }
}

#[cfg(unix)]
fn kiro_alive(pid: u32) -> bool {
    let Ok(p) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // kill(pid, 0): the process exists (lacking permission to signal also counts as existing)
    let exists = unsafe { libc::kill(p, 0) == 0 } || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !exists {
        return false;
    }
    // On Linux also check the program name; on other systems, where it's unavailable, assume it is
    match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(c) => c.to_lowercase().contains("kiro"),
        Err(_) => !std::path::Path::new("/proc/self").exists(),
    }
}

/// session/load reports the session doesn't exist (Cursor: `Session "<id>" not found`)
fn is_missing_session(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("no such session") || (m.contains("session") && (m.contains("not found") || m.contains("does not exist")))
}

/// models in the session/new and session/load results
pub fn parse_models(r: &Value) -> Vec<ModelOption> {
    r["models"]["availableModels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m["modelId"].as_str()?.to_string();
            Some(ModelOption {
                name: m["name"].as_str().unwrap_or(&id).to_string(),
                description: m["description"].as_str().unwrap_or("").to_string(),
                resolved: String::new(),
                efforts: vec![],
                default_effort: String::new(),
                id,
            })
        })
        .collect()
}

/// CodeBuddy turn failures don't use JSON-RPC errors: stopReason is refusal and the error goes in `_meta["codebuddy.ai/errorMessage"]` (a JSON string)
fn meta_error(r: &Value) -> Option<String> {
    let raw = r["_meta"].as_object()?.iter().find(|(k, _)| k.ends_with("errorMessage"))?.1.as_str()?;
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return Some(raw.to_string());
    };
    let msg = v["data"]["details"].as_str().or(v["message"].as_str()).unwrap_or(raw);
    Some(msg.to_string())
}

/// The tool's original tool name (Bash, Write…): Kiro and Qoder put it in `_meta.<tool>.toolName`
fn meta_tool_name(u: &Value) -> Option<&str> {
    let m = u["_meta"].as_object()?;
    m.values().find_map(|v| v["toolName"].as_str()).or_else(|| {
        m.iter().find(|(k, _)| k.ends_with("toolName")).and_then(|(_, v)| v.as_str())
    })
}

fn login_hint(spec: &StartSpec, msg: &str) -> String {
    let lower = msg.to_lowercase();
    if lower.contains("auth") || lower.contains("login") || msg.contains("401") {
        let cmd = match spec.tool {
            ToolKind::Cursor => "cursor-agent login",
            ToolKind::Kiro => "kiro-cli login",
            ToolKind::Qoder => "qodercli login",
            ToolKind::Codebuddy => "codebuddy`，在里面输入 `/login",
            _ => "登录命令",
        };
        format!("{msg}\n请先在这台机器上执行 `{cmd}` 完成登录")
    } else {
        msg.to_string()
    }
}

fn update(st: &mut State, em: &Emitter, u: &Value) {
    match u["sessionUpdate"].as_str().unwrap_or("") {
        "config_option_update" if st.parameterized => apply_config_options(st, u, em),
        "available_commands_update" => {
            let list = u["availableCommands"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| {
                    Some(SlashCommand {
                        name: c["name"].as_str()?.trim_start_matches('/').to_string(),
                        description: crate::util::truncate(c["description"].as_str().unwrap_or(""), 200),
                        hint: c["input"]["hint"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect();
            em.commands(list);
        }
        "current_mode_update" => {
            // The tool switched modes itself (e.g. leaving plan after planning)
            if let Some(m) = u["currentModeId"].as_str() {
                let plan = m.contains("plan");
                if plan != (st.perm == "plan") {
                    st.perm = if plan { "plan".into() } else { "default".into() };
                    em.permission_mode(&st.perm);
                }
            }
        }
        // Standard ACP context usage (CodeBuddy sends it, with this turn's credit cost in cost.amount)
        "usage_update" => {
            if let (Some(used), Some(size)) = (u["used"].as_f64(), u["size"].as_f64().filter(|s| *s > 0.0)) {
                if used > 0.0 {
                    em.context(used / size * 100.0);
                }
            }
            if !st.loading {
                if let Some(c) = u["cost"]["amount"].as_f64().filter(|c| *c > 0.0) {
                    st.turn_credits += c;
                }
            }
        }
        "agent_message_chunk" => chunk(st, em, Seg::Agent, &content_text(&u["content"])),
        "agent_thought_chunk" => chunk(st, em, Seg::Thought, &content_text(&u["content"])),
        "user_message_chunk" => chunk(st, em, Seg::User, &content_text(&u["content"])),
        "tool_call" | "tool_call_update" => {
            flush(st, em);
            if st.muted() {
                return;
            }
            let id = u["toolCallId"].as_str().unwrap_or("").to_string();
            if st.hidden.contains(&id) {
                return;
            }
            let known = st.tools.contains_key(&id);
            let mut t = st.tools.get(&id).cloned().unwrap_or_default();
            let before = (t.title.clone(), t.diffs.len(), t.input.clone());
            if let Some(title) = u["title"].as_str() {
                t.title = title.to_string();
            }
            if t.name.is_empty() {
                t.name = meta_tool_name(u)
                    .or(u["kind"].as_str())
                    .unwrap_or("tool")
                    .to_string();
            }
            if !u["rawInput"].is_null() {
                t.input = u["rawInput"].clone();
            }
            let is_agent = matches!(t.name.as_str(), "Agent" | "Task");
            if !known && !st.loading && !is_agent && !st.subagents.is_empty() {
                st.hidden.insert(id);
                return;
            }
            // Background subagents return at once, and the parent keeps talking while they run
            if is_agent && !t.done && !st.loading && t.input["run_in_background"] != json!(true) {
                st.subagents.insert(id.clone());
            }
            let mut texts = vec![];
            for c in u["content"].as_array().into_iter().flatten() {
                match c["type"].as_str().unwrap_or("") {
                    "diff" => {
                        let (old_text, new_text) = clean_diff_texts(
                            c["oldText"].as_str(),
                            c["newText"].as_str(),
                        );
                        let d = FileDiff {
                            path: c["path"].as_str().unwrap_or("").to_string(),
                            old_text,
                            new_text,
                            unified: None,
                        };
                        if !t.diffs.contains(&d) {
                            t.diffs.push(d);
                        }
                    }
                    "content" => texts.push(content_text(&c["content"])),
                    _ => {}
                }
            }
            let after = (t.title.clone(), t.diffs.len(), t.input.clone());
            if u["sessionUpdate"] == "tool_call" || before != after {
                em.event(EventBody::ToolCall {
                    id: id.clone(),
                    name: t.name.clone(),
                    title: t.title.clone(),
                    input: t.input.clone(),
                    diffs: t.diffs.clone(),
                });
            }
            let status = u["status"].as_str().unwrap_or("");
            if (status == "completed" || status == "failed") && !t.done {
                t.done = true;
                st.subagents.remove(&id);
                let mut output = texts.join("\n");
                if output.is_empty() && !u["rawOutput"].is_null() {
                    output = raw_output_text(&crate::computer_use::display_value(&u["rawOutput"]));
                }
                em.event(EventBody::ToolResult {
                    id: id.clone(),
                    output: crate::util::truncate(&output, 64 * 1024),
                    is_error: status == "failed",
                });
            }
            st.tools.insert(id, t);
        }
        "plan" => {
            flush(st, em);
            if st.muted() {
                return;
            }
            let entries = u["entries"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| PlanEntry {
                    content: e["content"].as_str().unwrap_or("").to_string(),
                    status: e["status"].as_str().unwrap_or("pending").to_string(),
                })
                .collect();
            em.event(EventBody::Plan { entries });
        }
        _ => {}
    }
}

/// Kiro's rawOutput looks like {"items":[{"Json":{"stdout":..,"stderr":..}}]}; extract readable text where possible
fn raw_output_text(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    // CodeBuddy: {"type":"text","text":"Command: dir\nStdout: …\nStderr: …\nExit Code: 0\nSignal: (none)"}
    if let Some(t) = v["text"].as_str() {
        return codebuddy_bash_text(t).unwrap_or_else(|| t.to_string());
    }
    let mut parts = vec![];
    for item in v["items"].as_array().into_iter().flatten() {
        let j = if item["Json"].is_null() { item } else { &item["Json"] };
        for k in ["stdout", "stderr"] {
            if let Some(s) = j[k].as_str().filter(|s| !s.is_empty()) {
                parts.push(s.to_string());
            }
        }
        if let Some(s) = item["Text"].as_str() {
            parts.push(s.to_string());
        }
        if parts.is_empty() {
            if let Some(s) = j["exit_status"].as_str() {
                parts.push(s.to_string());
            }
        }
    }
    if parts.is_empty() {
        v.to_string()
    } else {
        parts.join("\n")
    }
}

/// CodeBuddy command results are a "Command / Stdout / Stderr / Exit Code / Signal" text block; keep only the output,
/// and add an "exit code: N" line when the exit code isn't 0 (the frontend marks it failed based on that)
fn codebuddy_bash_text(t: &str) -> Option<String> {
    if !t.starts_with("Command:") {
        return None;
    }
    let field = |name: &str, next: &[&str]| -> Option<String> {
        let i = t.find(&format!("\n{name}:"))? + name.len() + 2;
        let rest = &t[i..];
        let end = next.iter().filter_map(|n| rest.find(&format!("\n{n}:"))).min().unwrap_or(rest.len());
        let v = rest[..end].trim();
        (!v.is_empty() && v != "(empty)" && v != "(none)").then(|| v.to_string())
    };
    let tail = ["Stderr", "Exit Code", "Signal", "Error", "Background PIDs", "Process Group PGID"];
    let mut parts: Vec<String> = [field("Stdout", &tail), field("Stderr", &tail[1..])].into_iter().flatten().collect();
    if let Some(c) = field("Exit Code", &tail[2..]).filter(|c| c != "0") {
        parts.push(format!("exit code: {c}"));
    }
    Some(parts.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::clean_diff_texts;

    #[test]
    fn desktop_notice_goes_with_the_first_real_prompt_only() {
        use super::*;
        let (_hub, em, _rx) = crate::hub::test_emitter();
        let mut st = State { desktop_notice: true, ..Default::default() };
        let ask = |text: &str| Prompt { text: text.into(), images: vec![] };
        let texts = |v: &Value| v["params"]["prompt"].as_array().unwrap().iter().map(|b| b["text"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let slash = prompt(&mut st, &em, &ask("/compact"));
        assert_eq!(texts(&slash), ["/compact"]);
        let first = prompt(&mut st, &em, &ask("open Feishu"));
        let first = texts(&first);
        assert!(first[0].contains("yi_computer_use") && first[0].contains("outdated"), "{first:?}");
        assert_eq!(first[1], "open Feishu");
        assert_eq!(texts(&prompt(&mut st, &em, &ask("again"))), ["again"]);
    }

    /// Qoder streams its subagents' text, thinking and tool calls as the parent's own updates
    #[test]
    fn subagent_updates_are_dropped() {
        use super::*;
        let (hub, em, _rx) = crate::hub::test_emitter();
        let mut st = State::default();
        let up = |st: &mut State, u: Value| update(st, &em, &u);
        let chunk = |kind: &str, t: &str| json!({"sessionUpdate": kind, "content": {"type": "text", "text": t}});
        let agent = |id: &str, status: Option<&str>, bg: bool| {
            let mut u = json!({"sessionUpdate": if status.is_some() { "tool_call_update" } else { "tool_call" }, "toolCallId": id,
                "title": "Agent", "_meta": {"qoder": {"toolName": "Agent"}}, "rawInput": {"description": "d", "run_in_background": bg}});
            if let Some(s) = status {
                u["status"] = json!(s);
                u["content"] = json!([{"type": "content", "content": {"type": "text", "text": format!("report {id}")}}]);
            }
            u
        };
        up(&mut st, chunk("agent_message_chunk", "Let me look."));
        up(&mut st, agent("a1", None, false));
        up(&mut st, agent("a2", None, false));
        // Two subagents interleaving, plus a tool call of theirs
        up(&mut st, chunk("agent_message_chunk", "and `mp"));
        up(&mut st, chunk("agent_thought_chunk", "Now"));
        up(&mut st, json!({"sessionUpdate": "tool_call", "toolCallId": "r1", "title": "Read x", "_meta": {"qoder": {"toolName": "Read"}}}));
        up(&mut st, chunk("agent_message_chunk", "-bsserver"));
        up(&mut st, json!({"sessionUpdate": "tool_call_update", "toolCallId": "r1", "status": "completed"}));
        up(&mut st, agent("a1", Some("completed"), false));
        up(&mut st, chunk("agent_message_chunk", "still a2"));
        up(&mut st, agent("a2", Some("completed"), false));
        // A background subagent returns at once and doesn't hold the parent's text back
        up(&mut st, agent("b1", None, true));
        up(&mut st, chunk("agent_message_chunk", "Summary."));
        up(&mut st, chunk("agent_thought_chunk", "done"));
        flush(&mut st, &em);
        let evs: Vec<String> = hub
            .store
            .events_since("s", 0, 100)
            .unwrap()
            .into_iter()
            .map(|e| match e.body {
                EventBody::AssistantText { text, .. } => format!("text:{text}"),
                EventBody::Thinking { text, .. } => format!("think:{text}"),
                EventBody::ToolCall { id, .. } => format!("call:{id}"),
                EventBody::ToolResult { id, output, .. } => format!("result:{id}:{output}"),
                _ => String::new(),
            })
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(
            evs,
            ["text:Let me look.", "call:a1", "call:a2", "result:a1:report a1", "result:a2:report a2", "call:b1", "text:Summary.", "think:done"]
        );
        // The end of a turn releases a subagent the tool never finished
        up(&mut st, agent("a3", None, false));
        st.end_subagents();
        assert!(st.subagents.is_empty());
    }

    #[test]
    fn codebuddy_bash() {
        let ok = serde_json::json!({"type":"text","text":"Command: dir\nStdout: a.txt\nb.txt\nStderr: (empty)\nExit Code: 0\nSignal: (none)"});
        assert_eq!(super::raw_output_text(&ok), "a.txt\nb.txt");
        let bad = serde_json::json!({"type":"text","text":"Command: x\nStdout: (empty)\nStderr: not found\nExit Code: 1\nSignal: (none)"});
        assert_eq!(super::raw_output_text(&bad), "not found\nexit code: 1");
    }

    #[test]
    fn cursor_new_file_diff() {
        let (o, n) = clean_diff_texts(Some("-- /dev/null"), Some("++ b//tmp/x/b.txt\nnew"));
        assert_eq!((o, n.as_deref()), (None, Some("new")));
        let (o, n) = clean_diff_texts(Some("a\n"), Some("++ b/not-a-header\n"));
        assert_eq!((o.as_deref(), n.as_deref()), (Some("a\n"), Some("++ b/not-a-header\n")));
    }
}

/// Qoder's ACP doesn't push usage_update; take the context ratio from the last record with context_usage_ratio in the session jsonl
fn qoder_context(sid: &str) -> Option<f64> {
    let path = crate::history::find_qoder_file(sid)?;
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines().rev() {
        if !line.contains("\"context_usage_ratio\"") {
            continue;
        }
        let v: Value = serde_json::from_str(line).ok()?;
        if let Some(r) = v["message"]["usage"]["context_usage_ratio"].as_f64().filter(|r| *r > 0.0) {
            return Some(r * 100.0);
        }
    }
    None
}

/// Cursor's ACP doesn't report token usage, so the context ratio can only be read from its local session store:
/// `~/.cursor/acp-sessions/<sessionId>/store.db` (terminal fallback: `chats/<md5(cwd)>/<sessionId>/store.db`), whose meta table holds hex-encoded JSON;
/// its latestRootBlobId points to the root record in the blobs table (protobuf, unencrypted),
/// and field 5 of the root record is token_details { 1: used_tokens, 2: max_tokens }
fn cursor_context(sid: &str) -> Option<f64> {
    cursor_context_in(&crate::paths::home().join(".cursor"), sid)
}

fn cursor_context_in(root: &std::path::Path, sid: &str) -> Option<f64> {
    // ACP writes a separate copy. A terminal-mode copy can be absent or stale after switching back to chat.
    let acp = root.join("acp-sessions").join(sid).join("store.db");
    let db = if acp.is_file() { acp } else {
        std::fs::read_dir(root.join("chats")).ok()?.flatten()
            .map(|d| d.path().join(sid).join("store.db")).find(|p| p.is_file())?
    };
    let conn = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let meta: String = conn
        .query_row("SELECT value FROM meta WHERE key = '0'", [], |r| r.get(0))
        .ok()?;
    let meta: Value = serde_json::from_slice(&unhex(meta.trim())?).ok()?;
    let root = meta["latestRootBlobId"].as_str()?;
    let blob: Vec<u8> = conn
        .query_row("SELECT data FROM blobs WHERE id = ?1", [root], |r| r.get(0))
        .ok()?;
    let details = pb_field(&blob, 5)?;
    let used = pb_varint(details, 1)? as f64;
    let max = pb_varint(details, 2)? as f64;
    (max > 0.0).then(|| used / max * 100.0)
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn read_varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut x = 0u64;
    for shift in (0..64).step_by(7) {
        let c = *b.get(*i)?;
        *i += 1;
        x |= u64::from(c & 0x7f) << shift;
        if c & 0x80 == 0 {
            return Some(x);
        }
    }
    None
}

/// Walk one level of protobuf, calling back for each field with (field number, wire type, varint value, length-delimited content)
fn pb_walk<'a>(b: &'a [u8], mut f: impl FnMut(u64, u64, u64, &'a [u8]) -> bool) -> Option<()> {
    let mut i = 0;
    while i < b.len() {
        let tag = read_varint(b, &mut i)?;
        let (no, wt) = (tag >> 3, tag & 7);
        let (v, bytes): (u64, &[u8]) = match wt {
            0 => (read_varint(b, &mut i)?, &[]),
            1 => {
                i += 8;
                (0, &[])
            }
            2 => {
                let len = read_varint(b, &mut i)? as usize;
                let s = b.get(i..i.checked_add(len)?)?;
                i += len;
                (0, s)
            }
            5 => {
                i += 4;
                (0, &[])
            }
            _ => return None,
        };
        if f(no, wt, v, bytes) {
            return Some(());
        }
    }
    Some(())
}

fn pb_field(b: &[u8], no: u64) -> Option<&[u8]> {
    let mut out = None;
    pb_walk(b, |n, wt, _, s| {
        (n == no && wt == 2).then(|| out = Some(s)).is_some()
    })?;
    out
}

fn pb_varint(b: &[u8], no: u64) -> Option<u64> {
    let mut out = None;
    pb_walk(b, |n, wt, v, _| (n == no && wt == 0).then(|| out = Some(v)).is_some())?;
    out
}

#[cfg(test)]
mod cursor_tests {
    use super::*;

    #[test]
    fn cursor_token_details() {
        // Start of field 5 of a root record from a real session store: used=31908, max=256000
        let details = [0x08, 0xa4, 0xf9, 0x01, 0x10, 0x80, 0xd0, 0x0f];
        let mut root = vec![0x0a, 0x02, 0xaa, 0xbb, 0x2a, details.len() as u8];
        root.extend_from_slice(&details);
        root.extend_from_slice(&[0x50, 0x01]);
        let d = pb_field(&root, 5).unwrap();
        assert_eq!(pb_varint(d, 1), Some(31908));
        assert_eq!(pb_varint(d, 2), Some(256000));
        assert_eq!(unhex("7b7d"), Some(b"{}".to_vec()));
    }
}

#[cfg(test)]
mod model_config_tests {
    use super::*;

    fn config() -> Value {
        json!({"configOptions":[
            {"id":"mode","category":"mode","currentValue":"agent"},
            {"id":"model","category":"model","currentValue":"gpt-test"},
            {"id":"fast","name":"Fast","category":"model_config","type":"select","currentValue":"false",
             "options":[{"value":"false","name":"Off"},{"value":"true","name":"Fast"}]},
            {"id":"reasoning","name":"Reasoning","category":"thought_level","type":"select","currentValue":"medium",
             "options":[{"value":"medium","name":"Medium"},{"value":"high","name":"High"}]}
        ]})
    }

    #[test]
    fn parameters_are_authoritative_and_requests_are_ordered() {
        let (hub, em, _rx) = crate::hub::test_emitter();
        let mut st = State { session_id: Some("ext".into()), parameterized: true, ..Default::default() };
        apply_config_options(&mut st, &config(), &em);
        assert_eq!(st.config_options.len(), 2);
        assert_eq!(hub.session_models("s").unwrap()["current"], "gpt-test");
        st.config_queue = vec![("fast".into(), "true".into()), ("reasoning".into(), "high".into())];
        let first = next_config(&mut st).unwrap();
        assert_eq!(first["method"], "session/set_config_option");
        assert_eq!(first["params"]["configId"], "fast");
        assert!(config_pending(&st));
        assert!(next_config(&mut st).is_none(), "do not overlap parameter requests");
        st.pending.remove(&first["id"].as_u64().unwrap());
        let mut ack = config();
        ack["configOptions"][2]["currentValue"] = json!("true");
        apply_config_options(&mut st, &ack, &em);
        assert_eq!(next_config(&mut st).unwrap()["params"]["configId"], "reasoning");
        let saved = hub.store.load_sessions().unwrap();
        assert_eq!(saved[0].model_config_options[0].current_value, "true");
    }

    #[test]
    fn unsupported_saved_parameters_do_not_block_prompts() {
        let mut st = State { session_id: Some("ext".into()), parameterized: true, ..Default::default() };
        st.config_queue = vec![("fast".into(), "true".into()), ("reasoning".into(), "ultra".into())];
        assert!(next_config(&mut st).is_none());
        assert!(!config_pending(&st));
        assert_eq!(client_capabilities(ToolKind::Cursor)["_meta"]["parameterizedModelPicker"], true);
        assert!(client_capabilities(ToolKind::Kiro).get("_meta").is_none());
    }

    #[test]
    fn context_prefers_acp_store_over_stale_terminal_copy() {
        let root = std::env::temp_dir().join(format!("yi-cursor-context-{}", uuid::Uuid::new_v4()));
        let write_store = |path: &std::path::Path, used: u8| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let conn = rusqlite::Connection::open(path).unwrap();
            conn.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT); CREATE TABLE blobs(id TEXT PRIMARY KEY, data BLOB);").unwrap();
            let meta = br#"{"latestRootBlobId":"root"}"#;
            let hex: String = meta.iter().map(|b| format!("{b:02x}")).collect();
            conn.execute("INSERT INTO meta VALUES ('0', ?1)", [hex]).unwrap();
            conn.execute("INSERT INTO blobs VALUES ('root', ?1)", [vec![0x2a, 4, 8, used, 16, 100]]).unwrap();
        };
        write_store(&root.join("chats/work/session/store.db"), 10);
        assert_eq!(cursor_context_in(&root, "session"), Some(10.0));
        write_store(&root.join("acp-sessions/session/store.db"), 70);
        assert_eq!(cursor_context_in(&root, "session"), Some(70.0));
        std::fs::remove_dir_all(root).unwrap();
    }
}
