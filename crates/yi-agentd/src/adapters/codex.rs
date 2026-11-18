//! Codex adapter: `codex app-server` (JSON-RPC over stdio).
//! Approvals arrive as server→client requests (item/*/requestApproval), answered with a decision.

use super::{AdapterCmd, JsonProc, Prompt, StartSpec};
use crate::hub::Emitter;
use yi_proto::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::mpsc;

/// Request ids we sent -> purpose
#[derive(Debug, Clone, Copy, PartialEq)]
enum Pending {
    Init,
    Thread,
    Turn,
    Models,
    Other,
}

struct PendingApproval {
    rpc_id: Value,
    method: String,
    params: Value,
    /// requestUserInput questions
    questions: Vec<Question>,
}

#[derive(Default)]
struct State {
    next_id: u64,
    pending: HashMap<u64, Pending>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    queue: Vec<Prompt>,
    /// Model switched during the session; sent with every turn/start afterwards
    model: Option<String>,
    /// Thinking level, sent with every turn/start
    effort: Option<String>,
    /// Permission mode switched during the session; every turn/start afterwards carries the matching approval policy and sandbox
    perm: Option<String>,
    approvals: HashMap<String, PendingApproval>,
    /// Previous cumulative tokens, used to compute increments
    last_total: Option<Value>,
    /// On thread/resume app-server replays history items; this switch decides whether to record them
    replaying: bool,
    /// This compaction is the thread/compact/start we sent (manual); otherwise Codex triggered it itself
    manual_compact: bool,
    /// Unanswered turn/steer: request id -> message. If it fails because the turn just ended, start a new turn instead
    steers: HashMap<u64, Prompt>,
    /// This run continues an existing thread (thread/resume was sent) rather than a new one; reset to false when resuming fails and we start a new one
    resumed: bool,
    /// Parameters for starting a new thread, reused when resuming fails and we start a new one
    thread_params: Value,
    /// Latest rateLimits from account/rateLimits/updated (Codex sends one right before a usage limit error)
    rate_limits: Value,
    /// This turn's error was usageLimitExceeded
    usage_limited: bool,
    /// Subagent threads (multi_agent `spawn_agent`), by thread id. app-server streams their notifications on this
    /// connection with their own threadId. Running ones are the session's background tasks
    subs: HashMap<String, SubThread>,
}

/// One subagent thread
#[derive(Default)]
struct SubThread {
    /// Id of the ToolCall that started it; its ToolResult is the subagent's reply
    spawn_call: Option<String>,
    /// Running turn
    turn: Option<String>,
    /// Last message of the running turn
    reply: String,
    /// Previous cumulative tokens of this thread
    last_total: Option<Value>,
    /// Name from the spawn (last segment of its agent path), shown in the background task list
    name: String,
    /// When its running turn started (ms)
    started_at: i64,
    /// Stopped from the background task list
    stopped: bool,
}

/// Id prefix for subagents' own tool calls (the timeline tags them "Subagent")
const SUB_PREFIX: &str = "cx-sub:";

impl State {
    fn req(&mut self, kind: Pending, method: &str, params: Value) -> Value {
        self.next_id += 1;
        self.pending.insert(self.next_id, kind);
        json!({"id": self.next_id, "method": method, "params": params})
    }
}

fn policy(mode: &str) -> (&'static str, &'static str) {
    match mode {
        "bypass" => ("never", "danger-full-access"),
        // Approve on request like default, but hand it to Codex's automatic reviewer (see reviewer)
        "plan" => ("untrusted", "read-only"),
        "strict" => ("untrusted", "workspace-write"),
        _ => ("on-request", "workspace-write"),
    }
}

/// Who handles approval requests: Codex's automatic reviewer in auto mode, the user otherwise
fn reviewer(mode: &str) -> &'static str {
    if mode == "auto" {
        "auto_review"
    } else {
        "user"
    }
}

/// thread/start sandbox value -> turn/start sandboxPolicy.type
fn sandbox_type(s: &str) -> &'static str {
    match s {
        "danger-full-access" => "dangerFullAccess",
        "read-only" => "readOnly",
        _ => "workspaceWrite",
    }
}

/// clientInfo.name for `codex app-server`. Codex sends it as the `originator` header on every model request, and some
/// API gateways only accept Codex's own clients, so use the Codex CLI's name (403 "restricted to ... codex" otherwise)
pub const CLIENT_NAME: &str = "codex_cli_rs";

pub async fn run(spec: StartSpec, em: Emitter, mut rx: mpsc::UnboundedReceiver<AdapterCmd>) {
    let mut args = vec!["app-server".to_string()];
    args.extend(spec.extra_args.iter().cloned());
    let mut p = match JsonProc::spawn_with_env(&spec.program, &args, &spec.cwd, &spec.env, &spec.env_unset) {
        Ok(p) => p,
        Err(e) => return em.exited(Some(format!("启动 codex 失败: {e}"))),
    };
    let mut st = State {
        effort: spec.effort.clone().filter(|e| !e.is_empty()),
        ..Default::default()
    };
    let init = st.req(
        Pending::Init,
        "initialize",
        json!({"clientInfo":{"name":CLIENT_NAME,"title":"Yi Console","version":DAEMON_VERSION},
               "capabilities":null}),
    );
    let _ = p.send(&init).await;

    let mut killed = false;
    loop {
        tokio::select! {
            line = p.stdout.next_line() => {
                let Ok(Some(line)) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                for out in handle(&mut st, &spec, &v, &em) {
                    let _ = p.send(&out).await;
                }
                if st.thread_id.is_some() && st.turn_id.is_none() && !st.queue.is_empty() {
                    let prompt = st.queue.remove(0);
                    let m = start_turn(&mut st, &prompt);
                    let _ = p.send(&m).await;
                }
            }
            cmd = rx.recv() => match cmd {
                Some(AdapterCmd::Send(prompt)) => {
                    em.status(SessionStatus::Running, None);
                    if st.thread_id.is_some() && st.turn_id.is_none() {
                        let m = start_turn(&mut st, &prompt);
                        let _ = p.send(&m).await;
                    } else if let (Some(tid), Some(turn)) = (st.thread_id.clone(), st.turn_id.clone().filter(|t| !t.is_empty() && !prompt.text.starts_with('/'))) {
                        // Turn in progress: use turn/steer to insert the message into the current turn
                        let m = st.req(Pending::Other, "turn/steer", json!({"threadId":tid,"expectedTurnId":turn,
                            "input":input_items(&prompt)}));
                        st.steers.insert(st.next_id, prompt);
                        let _ = p.send(&m).await;
                    } else {
                        st.queue.push(prompt);
                    }
                }
                Some(AdapterCmd::SetModel(model)) => {
                    // app-server sets the model per turn; it takes effect from the next turn/start
                    em.model(&model);
                    st.model = Some(model).filter(|m| !m.is_empty());
                }
                Some(AdapterCmd::SetPermissionMode(mode)) => {
                    st.perm = Some(mode);
                }
                Some(AdapterCmd::SetEffort(e)) => {
                    st.effort = Some(e).filter(|e| !e.is_empty());
                }
                Some(AdapterCmd::Interrupt) => {
                    if let (Some(t), Some(turn)) = (st.thread_id.clone(), st.turn_id.clone()) {
                        let m = st.req(Pending::Other, "turn/interrupt", json!({"threadId":t,"turnId":turn}));
                        let _ = p.send(&m).await;
                    }
                }
                Some(AdapterCmd::StopTask(task)) => {
                    // Background task ids are subagent thread ids
                    if let Some(m) = stop_sub(&mut st, &task) {
                        let _ = p.send(&m).await;
                    }
                }
                Some(AdapterCmd::Permission { request_id, option_id, answers }) => {
                    if let Some(a) = st.approvals.remove(&request_id) {
                        let result = if a.method == "item/tool/requestUserInput" {
                            user_input_result(&a, &option_id, &answers)
                        } else {
                            approval_result(&a, &option_id)
                        };
                        let _ = p.send(&json!({"id": a.rpc_id, "result": result})).await;
                        em.event(EventBody::PermissionResolved { request_id, option_id, answers });
                    }
                }
                Some(AdapterCmd::Kill) | None => { killed = true; break; }
                Some(_) => {}
            }
        }
    }
    let msg = p.finish(killed).await;
    em.exited(msg);
}

fn input_items(p: &Prompt) -> Vec<Value> {
    let mut items = vec![];
    if !p.text.trim().is_empty() {
        items.push(json!({"type":"text","text":p.text,"text_elements":[]}));
    }
    for i in &p.images {
        items.push(json!({"type":"localImage","path":i.path.display().to_string()}));
    }
    items
}

/// The / commands in the Codex terminal are implemented by the TUI and app-server doesn't recognize them; the common ones are wired up here
pub fn slash_commands() -> Vec<SlashCommand> {
    let c = |name: &str, description: &str, hint: &str| SlashCommand {
        name: name.into(),
        description: description.into(),
        hint: hint.into(),
    };
    vec![
        c("review", "代码审查：默认审查未提交的改动", "[branch <分支> | commit <sha> | 审查要求]"),
        c("init", "为这个仓库生成 AGENTS.md", ""),
        c("compact", "压缩上下文", ""),
    ]
}

const INIT_PROMPT: &str = "为这个仓库生成一份 AGENTS.md，作为给 AI 编码助手和新贡献者看的项目指南。\
先浏览目录结构、构建脚本和现有文档，再写：项目结构与模块划分、构建/测试/运行命令、代码风格与命名约定、\
测试约定、提交与 PR 约定，以及其他需要注意的事项。内容简洁具体，200～400 字为宜；已有 AGENTS.md 时在原文基础上补充，不要整篇重写。";

/// /review argument -> review/start target
fn review_target(arg: &str) -> Value {
    let (head, rest) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
    let rest = rest.trim();
    match head {
        "" => json!({"type":"uncommittedChanges"}),
        "branch" | "分支" if !rest.is_empty() => json!({"type":"baseBranch","branch":rest}),
        "commit" | "提交" if !rest.is_empty() => json!({"type":"commit","sha":rest}),
        _ => json!({"type":"custom","instructions":arg}),
    }
}

fn start_turn(st: &mut State, prompt: &Prompt) -> Value {
    let tid = st.thread_id.clone().unwrap_or_default();
    // Reserve the slot first, so another message arriving before turn/started doesn't start a second turn
    st.turn_id = Some(String::new());
    let text = prompt.text.trim();
    let (cmd, arg) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    match cmd {
        "/compact" => {
            st.manual_compact = true;
            return st.req(Pending::Turn, "thread/compact/start", json!({"threadId":tid}));
        }
        "/review" => {
            return st.req(Pending::Turn, "review/start",
                json!({"threadId":tid,"target":review_target(arg.trim()),"delivery":"inline"}))
        }
        "/init" => {
            let p = Prompt { text: format!("{INIT_PROMPT}\n{}", arg.trim()).trim().to_string(), images: vec![] };
            return start_turn_with(st, tid, &p);
        }
        _ => {}
    }
    start_turn_with(st, tid, prompt)
}

fn start_turn_with(st: &mut State, tid: String, prompt: &Prompt) -> Value {
    let mut params = json!({"threadId":tid,"input":input_items(prompt)});
    if let Some(m) = &st.model {
        params["model"] = json!(m);
    }
    if let Some(e) = &st.effort {
        params["effort"] = json!(e);
    }
    if let Some(mode) = &st.perm {
        let (approval, sandbox) = policy(mode);
        params["approvalPolicy"] = json!(approval);
        params["sandboxPolicy"] = json!({ "type": sandbox_type(sandbox) });
        params["approvalsReviewer"] = json!(reviewer(mode));
    }
    st.req(Pending::Turn, "turn/start", params)
}

/// model/list result
pub fn parse_models(r: &Value) -> Vec<ModelOption> {
    r["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| !m["hidden"].as_bool().unwrap_or(false))
        .filter_map(|m| {
            let id = m["model"].as_str().or(m["id"].as_str())?.to_string();
            Some(ModelOption {
                name: m["displayName"].as_str().unwrap_or(&id).to_string(),
                description: m["description"].as_str().unwrap_or("").to_string(),
                resolved: String::new(),
                efforts: m["supportedReasoningEfforts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e["reasoningEffort"].as_str().map(String::from))
                    .collect(),
                default_effort: m["defaultReasoningEffort"].as_str().unwrap_or("").to_string(),
                id,
            })
        })
        .collect()
}

/// requestUserInput answer: {answers: {question id: {answers: [option labels or free text]}}}; empty when skipped
fn user_input_result(a: &PendingApproval, option_id: &str, answers: &[QuestionAnswer]) -> Value {
    let mut map = serde_json::Map::new();
    if option_id == QUESTION_ANSWER {
        for q in &a.questions {
            let Some(ans) = answers.iter().find(|x| x.question_id == q.id) else { continue };
            let mut list: Vec<String> = ans
                .selected
                .iter()
                .map(|id| q.options.iter().find(|o| &o.id == id).map(|o| o.label.clone()).unwrap_or_else(|| id.clone()))
                .collect();
            if !ans.text.trim().is_empty() {
                list.push(ans.text.trim().to_string());
            }
            if !list.is_empty() {
                map.insert(q.id.clone(), json!({ "answers": list }));
            }
        }
    }
    json!({ "answers": map })
}

fn user_input_questions(params: &Value) -> Vec<Question> {
    params["questions"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|q| {
                    let options: Vec<QuestionOption> = q["options"]
                        .as_array()
                        .map(|o| {
                            o.iter()
                                .filter_map(|o| {
                                    let label = o["label"].as_str()?.to_string();
                                    Some(QuestionOption {
                                        id: label.clone(),
                                        label,
                                        description: o["description"].as_str().unwrap_or("").to_string(),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Question {
                        id: q["id"].as_str().unwrap_or("").to_string(),
                        header: q["header"].as_str().unwrap_or("").to_string(),
                        question: q["question"].as_str().unwrap_or("").to_string(),
                        // Questions without options can only be answered with free text
                        allow_other: q["isOther"].as_bool().unwrap_or(false) || options.is_empty(),
                        options,
                        multi_select: false,
                        secret: q["isSecret"].as_bool().unwrap_or(false),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn approval_result(a: &PendingApproval, option_id: &str) -> Value {
    if a.method == "item/permissions/requestApproval" {
        return match option_id {
            "accept" | "acceptForSession" => json!({
                "permissions": a.params["permissions"],
                "scope": if option_id == "accept" { "turn" } else { "session" }
            }),
            _ => json!({"permissions": {}, "scope": "turn"}),
        };
    }
    if a.method == "execCommandApproval" || a.method == "applyPatchApproval" {
        // Values from the older protocol
        let d = match option_id {
            "accept" => "approved",
            "acceptForSession" => "approved_for_session",
            "cancel" => "abort",
            _ => "denied",
        };
        return json!({ "decision": d });
    }
    json!({ "decision": option_id })
}

/// thread/resume can't find the thread record (rollout)
fn is_missing_thread(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("no rollout found") || (m.contains("thread") && m.contains("not found"))
}

fn handle(st: &mut State, spec: &StartSpec, v: &Value, em: &Emitter) -> Vec<Value> {
    let mut out = vec![];
    // 1) Responses to our requests
    if let (Some(id), None) = (v["id"].as_u64(), v.get("method")) {
        let kind = st.pending.remove(&id).unwrap_or(Pending::Other);
        let steer = st.steers.remove(&id);
        if let (Some(prompt), Some(_)) = (steer, v.get("error")) {
            // The turn ended just as we tried to steer: send it with the next turn
            st.queue.push(prompt);
            return out;
        }
        if let Some(err) = v.get("error") {
            // Older versions lack model/list; don't show that as an error
            if kind == Pending::Models {
                return out;
            }
            let msg = err["message"].as_str().unwrap_or("codex 返回错误").to_string();
            // If a thread was opened but no message was ever sent, Codex doesn't write a rollout file, so it can't be resumed once the process is gone (just created or just /clear-ed).
            // The old thread had no content anyway, so just start a new one
            if kind == Pending::Thread && st.resumed && is_missing_thread(&msg) {
                st.resumed = false;
                st.replaying = false;
                em.resume_fallback();
                let mut params = std::mem::take(&mut st.thread_params);
                if let Some(handoff) = spec.account_handoff.as_deref().filter(|s| !s.is_empty()) {
                    let existing = params["developerInstructions"].as_str().unwrap_or("");
                    let combined = if existing.is_empty() {
                        handoff.to_string()
                    } else {
                        format!("{existing}\n\n{handoff}")
                    };
                    params["developerInstructions"] = json!(combined);
                }
                out.push(st.req(Pending::Thread, "thread/start", params));
                return out;
            }
            em.event(EventBody::Error { message: msg.clone() });
            if kind == Pending::Turn {
                if is_usage_limit(&err["data"]) || super::is_usage_limit_text(&msg) {
                    em.event(super::limit_hit(limit_reset(&st.rate_limits), None));
                }
                st.turn_id = None;
                em.event(EventBody::TurnEnd {
                    stop_reason: Some("error".into()),
                    model: None,
                });
            }
            return out;
        }
        match kind {
            Pending::Init => {
                out.push(json!({"method":"initialized"}));
                out.push(st.req(Pending::Models, "model/list", json!({})));
                let (approval, sandbox) = policy(&spec.permission_mode);
                let mut params = json!({
                    "cwd": spec.cwd.display().to_string(),
                    "approvalPolicy": approval,
                    "sandbox": sandbox,
                });
                if let Some(m) = spec.model.as_ref().filter(|m| !m.is_empty()) {
                    params["model"] = json!(m);
                }
                // Overrides developer_instructions in config.toml
                params["developerInstructions"] = json!(super::system_text(spec));
                // Only sent for auto; older Codex versions don't have this field
                if spec.permission_mode == "auto" {
                    params["approvalsReviewer"] = json!("auto_review");
                }
                let msg = if let Some(tid) = &spec.resume {
                    st.thread_params = params.clone();
                    st.resumed = true;
                    params["threadId"] = json!(tid);
                    st.replaying = spec.replay;
                    st.req(Pending::Thread, "thread/resume", params)
                } else {
                    st.req(Pending::Thread, "thread/start", params)
                };
                out.push(msg);
            }
            Pending::Thread => {
                let th = &v["result"]["thread"];
                if let Some(tid) = th["id"].as_str() {
                    st.thread_id = Some(tid.to_string());
                    em.external_id(tid);
                }
                em.commands(slash_commands());
                if let Some(m) = v["result"]["model"].as_str() {
                    em.model(m);
                }
                if st.replaying {
                    // Subagent thread -> spawn call, to close the call when the history says the subagent finished
                    let mut spawned: HashMap<String, String> = HashMap::new();
                    for turn in th["turns"].as_array().into_iter().flatten() {
                        for item in turn["items"].as_array().into_iter().flatten() {
                            for (child, call) in spawned_threads(item, "") {
                                spawned.insert(child, call);
                            }
                            if item["type"] == "subAgentActivity" && item["kind"] != "started" {
                                if let Some(call) = item["agentThreadId"].as_str().and_then(|t| spawned.remove(t)) {
                                    em.event(EventBody::ToolResult {
                                        id: call,
                                        output: String::new(),
                                        is_error: item["kind"] == "interrupted",
                                    });
                                }
                            }
                            for ev in item_events(item, true) {
                                em.event(ev);
                            }
                        }
                    }
                }
                st.replaying = false;
                if st.queue.is_empty() {
                    em.status(SessionStatus::Idle, None);
                }
            }
            Pending::Turn => {
                if let Some(t) = v["result"]["turn"]["id"].as_str() {
                    st.turn_id = Some(t.to_string());
                }
            }
            Pending::Models => em.models(parse_models(&v["result"])),
            Pending::Other => {}
        }
        return out;
    }

    let method = v["method"].as_str().unwrap_or("");
    let params = &v["params"];

    // 2) Requests from the server (with an id)
    if !v["id"].is_null() {
        let rid = v["id"].to_string();
        match method {
            "item/commandExecution/requestApproval" | "execCommandApproval" => {
                let cmd = params["command"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| {
                        params["command"].as_array().map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str())
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                    })
                    .unwrap_or_default();
                let reason = params["reason"].as_str().unwrap_or("");
                let title = if reason.is_empty() {
                    format!("执行命令: {cmd}")
                } else {
                    format!("执行命令: {cmd}（{reason}）")
                };
                ask(st, em, v, method, rid, "shell", title, codex_options(true));
            }
            "item/fileChange/requestApproval" | "applyPatchApproval"
                if st.perm.as_deref().unwrap_or(&spec.permission_mode) == "acceptEdits" =>
            {
                // Accept-edits mode: file edits are allowed right away
                let a = PendingApproval {
                    rpc_id: v["id"].clone(),
                    method: method.to_string(),
                    params: params.clone(),
                    questions: vec![],
                };
                out.push(json!({"id": v["id"], "result": approval_result(&a, "accept")}));
            }
            "item/fileChange/requestApproval" | "applyPatchApproval" => {
                let reason = params["reason"].as_str().unwrap_or("");
                ask(
                    st,
                    em,
                    v,
                    method,
                    rid,
                    "apply_patch",
                    format!("修改文件{}", if reason.is_empty() { String::new() } else { format!("（{reason}）") }),
                    codex_options(true),
                );
            }
            "item/permissions/requestApproval" => {
                let reason = params["reason"].as_str().unwrap_or("申请额外权限");
                ask(
                    st,
                    em,
                    v,
                    method,
                    rid,
                    "permissions",
                    reason.to_string(),
                    codex_options(true),
                );
            }
            "mcpServer/elicitation/request" => {
                out.push(json!({"id": v["id"], "result": {"action":"decline","content":null,"_meta":null}}));
            }
            "item/tool/requestUserInput" => {
                let questions = user_input_questions(params);
                st.approvals.insert(
                    rid.clone(),
                    PendingApproval {
                        rpc_id: v["id"].clone(),
                        method: method.to_string(),
                        params: params.clone(),
                        questions: questions.clone(),
                    },
                );
                em.event(EventBody::PermissionRequest {
                    request_id: rid,
                    tool_name: "request_user_input".into(),
                    title: "Codex 有问题想问你".into(),
                    input: params.clone(),
                    options: super::question_options(),
                    questions,
                });
            }
            _ => {
                out.push(json!({"id": v["id"], "error": {"code": -32601, "message": "not supported by yi-console"}}));
            }
        }
        return out;
    }

    // 3) Notifications
    if let Some(sub) = sub_thread(st, params) {
        sub_notification(st, &sub, method, params, em);
        return out;
    }
    match method {
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
            if let (Some(id), Some(d)) = (params["itemId"].as_str(), params["delta"].as_str()) {
                em.event(EventBody::ThinkingDelta {
                    id: id.to_string(),
                    delta: d.to_string(),
                });
            }
        }
        "item/agentMessage/delta" => {
            if let (Some(id), Some(d)) = (params["itemId"].as_str(), params["delta"].as_str()) {
                em.event(EventBody::AssistantDelta {
                    id: id.to_string(),
                    delta: d.to_string(),
                });
            }
        }
        "item/started" => {
            if params["item"]["type"] == "contextCompaction" {
                em.event(EventBody::Compact {
                    phase: "start".into(),
                    trigger: None,
                    pre_tokens: None,
                    post_tokens: None,
                    duration_ms: None,
                });
            }
            register_spawns(st, &params["item"], "");
            for ev in item_events(&params["item"], false) {
                if matches!(ev, EventBody::ToolCall { .. }) {
                    em.event(ev);
                }
            }
        }
        "item/completed" => {
            let item = &params["item"];
            register_spawns(st, item, "");
            // Commands/patches already emitted a ToolCall at item/started; only the result is needed here, and a patch's diff is taken from completion
            let started_already = matches!(
                item["type"].as_str(),
                Some("commandExecution") | Some("mcpToolCall") | Some("collabAgentToolCall") | Some("subAgentActivity")
            );
            let manual = item["type"] == "contextCompaction" && std::mem::take(&mut st.manual_compact);
            for mut ev in item_events(item, false) {
                if started_already && matches!(ev, EventBody::ToolCall { .. }) {
                    continue;
                }
                if let EventBody::Compact { trigger, .. } = &mut ev {
                    *trigger = Some(if manual { "manual" } else { "auto" }.into());
                }
                em.event(ev);
            }
        }
        "turn/started" => {
            st.usage_limited = false;
            if let Some(t) = params["turn"]["id"].as_str() {
                st.turn_id = Some(t.to_string());
            }
            em.status(SessionStatus::Running, None);
        }
        "turn/completed" => {
            st.turn_id = None;
            let turn = &params["turn"];
            if turn["status"] == "failed" {
                let msg = turn["error"]["message"].as_str().unwrap_or("");
                if !msg.is_empty() {
                    em.event(EventBody::Error {
                        message: msg.to_string(),
                    });
                }
                if std::mem::take(&mut st.usage_limited) || is_usage_limit(&turn["error"]) || super::is_usage_limit_text(msg) {
                    em.event(super::limit_hit(limit_reset(&st.rate_limits), None));
                }
            }
            // Subagents still running carry on as background tasks
            em.event(EventBody::TurnEnd {
                stop_reason: turn["status"].as_str().map(str::to_string),
                model: None,
            });
        }
        "turn/plan/updated" => {
            let entries = params["plan"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|s| PlanEntry {
                    content: s["step"].as_str().unwrap_or("").to_string(),
                    status: match s["status"].as_str().unwrap_or("") {
                        "inProgress" => "in_progress",
                        "completed" => "completed",
                        _ => "pending",
                    }
                    .to_string(),
                })
                .collect();
            em.event(EventBody::Plan { entries, session_scoped: false });
        }
        "thread/tokenUsage/updated" => {
            let tu = &params["tokenUsage"];
            if let Some(win) = tu["modelContextWindow"].as_f64().filter(|w| *w > 0.0) {
                let last = &tu["last"];
                let used = last["totalTokens"].as_f64().unwrap_or_else(|| {
                    last["inputTokens"].as_f64().unwrap_or(0.0) + last["outputTokens"].as_f64().unwrap_or(0.0)
                });
                em.context(used / win * 100.0);
            }
            let total = params["tokenUsage"]["total"].clone();
            let prev = st.last_total.replace(total.clone());
            // The first report after resume is the historical total; don't count it as usage for this run
            if prev.is_none() && st.resumed {
                return out;
            }
            em.event(usage_delta(&total, prev.as_ref()));
        }
        "account/rateLimits/updated" => st.rate_limits = params["rateLimits"].clone(),
        "error" => {
            if is_usage_limit(&params["error"]) && !params["willRetry"].as_bool().unwrap_or(false) {
                st.usage_limited = true;
            }
            if !params["willRetry"].as_bool().unwrap_or(false) {
                if let Some(m) = params["error"]["message"].as_str() {
                    em.event(EventBody::Error {
                        message: m.to_string(),
                    });
                }
            }
        }
        "thread/name/updated" => {
            if let Some(n) = params["name"].as_str().or(params["threadName"].as_str()) {
                em.title_if_empty(n);
            }
        }
        _ => {}
    }
    out
}

/// Usage event for the increase from `prev` to `total` (cumulative thread totals)
fn usage_delta(total: &Value, prev: Option<&Value>) -> EventBody {
    let g = |v: &Value, k: &str| v[k].as_u64().unwrap_or(0);
    let d = |k: &str| g(total, k).saturating_sub(prev.map(|p| g(p, k)).unwrap_or(0));
    let cached = d("cachedInputTokens");
    EventBody::Usage {
        usage: Usage {
            input_tokens: d("inputTokens").saturating_sub(cached),
            output_tokens: d("outputTokens"),
            cache_read_tokens: cached,
            cache_write_tokens: d("cacheWriteInputTokens"),
            cost_usd: 0.0,
            credits: 0.0,
        },
    }
}

/// The notification's thread when it is a subagent's rather than the main thread's
fn sub_thread(st: &State, params: &Value) -> Option<String> {
    let t = params["threadId"].as_str()?;
    let main = st.thread_id.as_deref()?;
    (t != main).then(|| t.to_string())
}

/// Subagent threads an item starts, with the id of the ToolCall that stands for each (prefixed when the spawning
/// thread is itself a subagent). multi_agent reports a spawn as `subAgentActivity` (kind started, call id as item id);
/// a `collabAgentToolCall` spawnAgent names them in receiverThreadIds
fn spawned_threads(item: &Value, pre: &str) -> Vec<(String, String)> {
    let id = format!("{pre}{}", item["id"].as_str().unwrap_or(""));
    match item["type"].as_str() {
        Some("subAgentActivity") if item["kind"] == "started" => {
            item["agentThreadId"].as_str().map(|t| vec![(t.to_string(), id)]).unwrap_or_default()
        }
        Some("collabAgentToolCall") if item["tool"] == "spawnAgent" => item["receiverThreadIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t.as_str().map(|t| (t.to_string(), id.clone())))
            .collect(),
        _ => vec![],
    }
}

fn register_spawns(st: &mut State, item: &Value, pre: &str) {
    for (child, call) in spawned_threads(item, pre) {
        let s = st.subs.entry(child).or_default();
        s.spawn_call.get_or_insert(call);
        if s.name.is_empty() {
            s.name = item["agentPath"].as_str().and_then(|p| p.rsplit('/').next()).unwrap_or("").to_string();
        }
    }
}

/// A subagent thread's notification. Its tool calls are shown (ids with SUB_PREFIX), its last message becomes the
/// spawn call's result, its tokens count as usage; text deltas, reasoning, plans and its turn events stay out of the
/// main timeline. A running subagent is a background task, like Claude's: the main turn ends on its own schedule
fn sub_notification(st: &mut State, sub: &str, method: &str, params: &Value, em: &Emitter) {
    match method {
        "turn/started" => {
            let s = st.subs.entry(sub.to_string()).or_default();
            s.turn = params["turn"]["id"].as_str().map(str::to_string).or(Some(String::new()));
            s.reply.clear();
            s.stopped = false;
            s.started_at = crate::util::now_ms();
            publish_tasks(st, em);
        }
        "turn/completed" => {
            let s = st.subs.entry(sub.to_string()).or_default();
            s.turn = None;
            let turn = &params["turn"];
            let status = turn["status"].as_str().unwrap_or("completed");
            let reply = std::mem::take(&mut s.reply);
            let stopped = std::mem::take(&mut s.stopped);
            let name = if s.name.is_empty() { sub.to_string() } else { s.name.clone() };
            // Only the first turn answers the spawn call; later turns (follow-up input) show their tool calls only
            if let Some(call) = s.spawn_call.take() {
                let err = turn["error"]["message"].as_str().unwrap_or("");
                em.event(EventBody::ToolResult {
                    id: call,
                    output: if reply.is_empty() { err.to_string() } else { reply },
                    is_error: status != "completed",
                });
            }
            // While the main turn waits for it, its result above says enough; otherwise leave a line, as for
            // Claude's background tasks (format parsed by ChatView)
            if st.turn_id.is_none() {
                let what = match status {
                    _ if stopped || status == "interrupted" => "已停止",
                    "completed" => "完成",
                    _ => "失败",
                };
                em.event(EventBody::System { text: format!("后台任务{what}：子 agent {name}") });
            }
            publish_tasks(st, em);
        }
        "item/started" | "item/completed" => {
            let item = &params["item"];
            register_spawns(st, item, SUB_PREFIX);
            if item["type"] == "agentMessage" {
                let text = item["text"].as_str().unwrap_or("").trim();
                if method == "item/completed" && !text.is_empty() {
                    st.subs.entry(sub.to_string()).or_default().reply = text.to_string();
                }
                return;
            }
            let started = method == "item/started";
            let started_already = matches!(
                item["type"].as_str(),
                Some("commandExecution") | Some("mcpToolCall") | Some("collabAgentToolCall") | Some("subAgentActivity")
            );
            for ev in item_events(item, false) {
                let ev = match ev {
                    EventBody::ToolCall { id, name, title, input, diffs } if started || !started_already => {
                        EventBody::ToolCall { id: format!("{SUB_PREFIX}{id}"), name, title, input, diffs }
                    }
                    EventBody::ToolResult { id, output, is_error } if !started => {
                        EventBody::ToolResult { id: format!("{SUB_PREFIX}{id}"), output, is_error }
                    }
                    _ => continue,
                };
                em.event(ev);
            }
        }
        "thread/tokenUsage/updated" => {
            let total = params["tokenUsage"]["total"].clone();
            let s = st.subs.entry(sub.to_string()).or_default();
            let prev = s.last_total.replace(total.clone());
            em.event(usage_delta(&total, prev.as_ref()));
        }
        _ => {}
    }
}

/// Running subagents as the session's background tasks
fn publish_tasks(st: &State, em: &Emitter) {
    let mut tasks: Vec<BackgroundTask> = st
        .subs
        .iter()
        .filter(|(_, s)| s.turn.is_some())
        .map(|(id, s)| BackgroundTask {
            id: id.clone(),
            kind: "local_agent".into(),
            description: if s.name.is_empty() { id.clone() } else { s.name.clone() },
            started_at: s.started_at,
            output_file: None,
            unstoppable: false,
        })
        .collect();
    tasks.sort_by(|a, b| a.started_at.cmp(&b.started_at).then_with(|| a.id.cmp(&b.id)));
    em.background_tasks(tasks);
}

/// turn/interrupt for a subagent's running turn (stopped from the background task list)
fn stop_sub(st: &mut State, thread: &str) -> Option<Value> {
    let s = st.subs.get_mut(thread)?;
    let turn = s.turn.clone().filter(|t| !t.is_empty())?;
    s.stopped = true;
    Some(st.req(Pending::Other, "turn/interrupt", json!({"threadId":thread,"turnId":turn})))
}

#[allow(clippy::too_many_arguments)]
fn ask(
    st: &mut State,
    em: &Emitter,
    v: &Value,
    method: &str,
    rid: String,
    tool: &str,
    title: String,
    options: Vec<PermissionOption>,
) {
    st.approvals.insert(
        rid.clone(),
        PendingApproval {
            rpc_id: v["id"].clone(),
            method: method.to_string(),
            params: v["params"].clone(),
            questions: vec![],
        },
    );
    em.event(EventBody::PermissionRequest {
        request_id: rid,
        tool_name: tool.to_string(),
        title,
        input: v["params"].clone(),
        options,
        questions: vec![],
    });
}

fn codex_options(session: bool) -> Vec<PermissionOption> {
    let mut v = vec![PermissionOption {
        id: "accept".into(),
        label: "允许".into(),
        kind: "allow_once".into(),
    }];
    if session {
        v.push(PermissionOption {
            id: "acceptForSession".into(),
            label: "本会话内允许".into(),
            kind: "allow_always".into(),
        });
    }
    v.push(PermissionOption {
        id: "decline".into(),
        label: "拒绝".into(),
        kind: "reject_once".into(),
    });
    v.push(PermissionOption {
        id: "cancel".into(),
        label: "拒绝并停止".into(),
        kind: "reject_always".into(),
    });
    v
}

/// `AsyncUserInputQuestion[]` ({title, options?: string[]}) -> questions. They have no ids, so they are numbered;
/// option ids are the labels, and free text is always allowed (Codex's tool description promises that)
fn async_questions(v: &Value) -> Vec<Question> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|q| Some((q["title"].as_str().map(str::trim).filter(|t| !t.is_empty())?, q)))
        .enumerate()
        .map(|(i, (title, q))| Question {
            id: (i + 1).to_string(),
            question: title.to_string(),
            options: q["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| o.as_str().map(str::trim).filter(|o| !o.is_empty()))
                .map(|o| QuestionOption { id: o.to_string(), label: o.to_string(), description: String::new() })
                .collect(),
            allow_other: true,
            ..Default::default()
        })
        .collect()
}

/// app-server ThreadItem -> unified events. With `with_user` true, user messages are emitted too (for replaying history)
pub fn item_events(item: &Value, with_user: bool) -> Vec<EventBody> {
    let id = item["id"].as_str().unwrap_or("").to_string();
    let s = |k: &str| item[k].as_str().unwrap_or("").to_string();
    match item["type"].as_str().unwrap_or("") {
        "userMessage" if with_user => {
            let text = item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            vec![EventBody::UserMessage { text, images: vec![], origin: None }]
        }
        "agentMessage" => {
            let text = s("text");
            let mut out = vec![];
            if !text.trim().is_empty() {
                out.push(EventBody::AssistantText { id: id.clone(), text });
            }
            // request_user_input_async: the tool returns at once and the turn goes on; the questions ride on this message
            let questions = if item["delivery"] == "async" { async_questions(&item["questions"]) } else { vec![] };
            if !questions.is_empty() {
                out.push(EventBody::AsyncQuestion { id, questions });
            }
            out
        }
        "reasoning" => {
            let mut parts: Vec<String> = item["summary"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect();
            if parts.is_empty() {
                parts = item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect();
            }
            let text = parts.join("\n\n");
            if text.trim().is_empty() {
                vec![]
            } else {
                vec![EventBody::Thinking { id, text }]
            }
        }
        "commandExecution" => {
            let mut v = vec![EventBody::ToolCall {
                id: id.clone(),
                name: "shell".into(),
                title: crate::util::truncate(&strip_shell_wrapper(&s("command")), 200),
                input: json!({"command": s("command"), "cwd": s("cwd")}),
                diffs: vec![],
            }];
            let status = s("status");
            if status == "completed" || status == "failed" || status == "declined" {
                let exit = item["exitCode"].as_i64();
                v.push(EventBody::ToolResult {
                    id,
                    output: crate::util::truncate(
                        item["aggregatedOutput"].as_str().unwrap_or(""),
                        64 * 1024,
                    ),
                    is_error: status != "completed" || exit.map(|c| c != 0).unwrap_or(false),
                });
            }
            v
        }
        "fileChange" => {
            let diffs: Vec<FileDiff> = item["changes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| FileDiff {
                    path: c["path"].as_str().unwrap_or("").to_string(),
                    old_text: None,
                    new_text: None,
                    unified: c["diff"].as_str().map(str::to_string),
                })
                .collect();
            let title = diffs
                .iter()
                .map(|d| d.path.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let status = s("status");
            let mut v = vec![EventBody::ToolCall {
                id: id.clone(),
                name: "apply_patch".into(),
                title,
                input: Value::Null,
                diffs,
            }];
            if !status.is_empty() && status != "inProgress" {
                v.push(EventBody::ToolResult {
                    id,
                    output: status.clone(),
                    is_error: status != "completed",
                });
            }
            v
        }
        "mcpToolCall" => {
            let name = format!("{}.{}", s("server"), s("tool"));
            let mut v = vec![EventBody::ToolCall {
                id: id.clone(),
                title: crate::util::truncate(&item["arguments"].to_string(), 200),
                name,
                input: item["arguments"].clone(),
                diffs: vec![],
            }];
            let status = s("status");
            if status == "completed" || status == "failed" {
                v.push(EventBody::ToolResult {
                    id,
                    output: if s("server") == "yi_computer_use" { crate::computer_use::display_result(&item["result"]) } else { crate::util::truncate(&item["result"].to_string(), 64 * 1024) },
                    is_error: status == "failed",
                });
            }
            v
        }
        "subAgentActivity" if item["kind"] == "started" => {
            let path = s("agentPath");
            let name = path.rsplit('/').next().unwrap_or("").to_string();
            vec![EventBody::ToolCall {
                id,
                name: "spawn_agent".into(),
                title: name.clone(),
                input: json!({"description": name, "agentPath": path, "agentThreadId": s("agentThreadId"),
                    "model": item["model"], "reasoningEffort": item["reasoningEffort"]}),
                diffs: vec![],
            }]
        }
        "collabAgentToolCall" => {
            let tool = s("tool");
            let name = match tool.as_str() {
                "spawnAgent" => "spawn_agent".to_string(),
                "wait" => "wait_agent".to_string(),
                t => {
                    // camelCase -> snake_case
                    let mut n = String::new();
                    for c in t.chars() {
                        if c.is_ascii_uppercase() {
                            n.push('_');
                        }
                        n.push(c.to_ascii_lowercase());
                    }
                    n
                }
            };
            let prompt = s("prompt");
            let mut v = vec![EventBody::ToolCall {
                id: id.clone(),
                name,
                title: crate::util::truncate(&prompt, 200),
                // `wait` usually carries nothing: leave the input out instead of showing empty fields
                input: {
                    let mut o = serde_json::Map::new();
                    if !prompt.is_empty() {
                        o.insert("prompt".into(), json!(prompt));
                    }
                    if item["receiverThreadIds"].as_array().is_some_and(|a| !a.is_empty()) {
                        o.insert("receiverThreadIds".into(), item["receiverThreadIds"].clone());
                    }
                    if item["model"].is_string() {
                        o.insert("model".into(), item["model"].clone());
                    }
                    if o.is_empty() { Value::Null } else { Value::Object(o) }
                },
                diffs: vec![],
            }];
            let status = s("status");
            // A spawn is answered by the subagent's reply (sub_notification), unless it failed
            if !status.is_empty() && status != "inProgress" && (tool != "spawnAgent" || status != "completed") {
                let states = item["agentsStates"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(_, a)| a["message"].as_str().filter(|m| !m.is_empty()))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                v.push(EventBody::ToolResult { id, output: states, is_error: status != "completed" });
            }
            v
        }
        "webSearch" => vec![EventBody::ToolCall {
            id,
            name: "web_search".into(),
            title: s("query"),
            input: item.clone(),
            diffs: vec![],
        }],
        "plan" => vec![EventBody::System { text: s("text") }],
        "contextCompaction" => vec![EventBody::Compact {
            phase: "done".into(),
            trigger: None,
            pre_tokens: None,
            post_tokens: None,
            duration_ms: None,
        }],
        "enteredReviewMode" => vec![EventBody::System { text: format!("开始代码审查：{}", s("review")) }],
        _ => vec![],
    }
}

/// `/bin/bash -lc 'touch x'` -> `touch x`
pub(crate) fn strip_shell_wrapper(cmd: &str) -> String {
    for prefix in ["/bin/bash -lc ", "bash -lc ", "/bin/zsh -lc ", "/bin/sh -c "] {
        if let Some(rest) = cmd.strip_prefix(prefix) {
            let r = rest.trim();
            if r.len() >= 2 && r.starts_with('\'') && r.ends_with('\'') {
                return r[1..r.len() - 1].to_string();
            }
            return r.to_string();
        }
    }
    cmd.to_string()
}

/// TurnError.codexErrorInfo is "usageLimitExceeded" (also covers QuotaExceeded / UsageNotIncluded, which have no reset time)
fn is_usage_limit(err: &Value) -> bool {
    err["codexErrorInfo"] == "usageLimitExceeded"
}

/// When the exhausted window resets (ms): the latest resetsAt among windows at 100%. None if no window is full
/// (credits depleted, plan doesn't include usage), and the hub falls back to the quota probe
fn limit_reset(rl: &Value) -> Option<i64> {
    ["primary", "secondary"]
        .iter()
        .map(|k| &rl[*k])
        .filter(|w| w["usedPercent"].as_f64().is_some_and(|p| p >= 100.0))
        .filter_map(|w| w["resetsAt"].as_i64())
        .max()
        .map(|s| s * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn async_user_input_questions() {
        // Shape from codex-cli 0.166 app-server after the model calls request_user_input_async
        let item = json!({"type":"agentMessage","id":"call_1","text":"合盖 30 秒后告诉我\n- 已合盖\n- 不方便",
            "phase":"final_answer","delivery":"async",
            "questions":[{"title":"合盖 30 秒后告诉我","options":["已合盖","不方便"]},{"title":" "},{"title":"备注？"}]});
        let evs = item_events(&item, false);
        assert!(matches!(&evs[0], EventBody::AssistantText { id, .. } if id == "call_1"));
        let EventBody::AsyncQuestion { id, questions } = &evs[1] else { panic!("{evs:?}") };
        assert_eq!(id, "call_1");
        assert_eq!(questions.len(), 2);
        assert_eq!((questions[0].id.as_str(), questions[0].options.len()), ("1", 2));
        assert_eq!(questions[0].options[1].label, "不方便");
        assert!(questions[1].options.is_empty() && questions[1].allow_other);
        // Without delivery=async the questions are ignored
        let plain = json!({"type":"agentMessage","id":"m","text":"hi","questions":[{"title":"x"}]});
        assert_eq!(item_events(&plain, false).len(), 1);
    }
    #[test]
    fn usage_limit_reset() {
        // Shapes from codex-rs app-server-protocol v2 (RateLimitSnapshot / TurnError)
        let rl = json!({"limitId":"codex","primary":{"usedPercent":100,"windowDurationMins":300,"resetsAt":1793177589},
            "secondary":{"usedPercent":63,"windowDurationMins":10080,"resetsAt":1793604850}});
        assert_eq!(limit_reset(&rl), Some(1793177589000));
        let both = json!({"primary":{"usedPercent":100,"resetsAt":10},"secondary":{"usedPercent":100,"resetsAt":20}});
        assert_eq!(limit_reset(&both), Some(20000));
        assert_eq!(limit_reset(&json!({"primary":{"usedPercent":40,"resetsAt":10}})), None);
        assert_eq!(limit_reset(&Value::Null), None);
        assert!(is_usage_limit(&json!({"message":"x","codexErrorInfo":"usageLimitExceeded"})));
        assert!(!is_usage_limit(&json!({"codexErrorInfo":{"httpConnectionFailed":{"httpStatusCode":429}}})));
        assert!(crate::adapters::is_usage_limit_text("You’ve hit your usage limit. Try again at 3:05 PM."));
    }

    fn spec() -> StartSpec {
        StartSpec {
            tool: ToolKind::Codex,
            mode: SessionMode::Structured,
            cwd: "/tmp".into(),
            resume: None,
            replay: false,
            replay_skip: 0,
            model: None,
            effort: None,
            model_config_options: vec![],
            permission_mode: "default".into(),
            command: None,
            program: "codex".into(),
            extra_args: vec![],
            mcp_servers: vec![],
            mcp_fingerprint: String::new(),
            allow_mcp: vec![],
            system_prompt: None,
            system_notice: false,
            cols: 80,
            rows: 24,
            tmux: None,
            session_id: None,
            env: Default::default(),
            env_unset: vec![],
            data_dir: None,
            auth_method: None,
            account_handoff: None,
        }
    }

    fn note(method: &str, thread: &str, extra: Value) -> Value {
        let mut params = json!({"threadId": thread});
        for (k, v) in extra.as_object().unwrap() {
            params[k] = v.clone();
        }
        json!({"method": method, "params": params})
    }

    /// Shapes from codex-cli 0.166 app-server (multi_agent): the parent starts a subagent and ends its turn without waiting
    #[test]
    fn subagent_after_main_turn() {
        let (hub, em, _rx) = crate::hub::test_emitter();
        let sp = spec();
        let mut st = State { thread_id: Some("main".into()), ..Default::default() };
        let tu = |n: u64| json!({"tokenUsage":{"total":{"inputTokens":n,"cachedInputTokens":0,"outputTokens":10},"last":{},"modelContextWindow":1000}});
        let tasks = || hub.info("s").unwrap().background_tasks;
        let seq = vec![
            note("turn/started", "main", json!({"turn":{"id":"t1"}})),
            note("item/started", "main", json!({"item":{"type":"subAgentActivity","id":"call_1","kind":"started","agentThreadId":"sub","agentPath":"/root/late_ok","model":"m"}})),
            note("item/completed", "main", json!({"item":{"type":"subAgentActivity","id":"call_1","kind":"started","agentThreadId":"sub","agentPath":"/root/late_ok","model":"m"}})),
            note("turn/started", "sub", json!({"turn":{"id":"st1","rootTurnId":"t1"}})),
            note("item/completed", "main", json!({"item":{"type":"agentMessage","id":"m1","text":"已启动"}})),
            note("turn/completed", "main", json!({"turn":{"id":"t1","status":"completed"}})),
        ];
        for v in &seq {
            assert!(handle(&mut st, &sp, v, &em).is_empty());
        }
        // The main turn ends right away; the subagent's turn didn't touch turn_id and is a background task now
        assert!(st.turn_id.is_none());
        let evs = || hub.store.events_since("s", 0, 100).unwrap();
        assert_eq!(evs().iter().filter(|e| matches!(e.body, EventBody::TurnEnd { .. })).count(), 1);
        let t = tasks();
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].id.as_str(), t[0].kind.as_str(), t[0].description.as_str()), ("sub", "local_agent", "late_ok"));
        for v in [
            note("item/started", "sub", json!({"item":{"type":"commandExecution","id":"exec-1","command":"/bin/bash -lc 'sleep 15; echo LATE-OK'","status":"inProgress"}})),
            note("item/agentMessage/delta", "sub", json!({"itemId":"m2","delta":"x"})),
            note("item/completed", "sub", json!({"item":{"type":"commandExecution","id":"exec-1","command":"/bin/bash -lc 'sleep 15; echo LATE-OK'","status":"completed","aggregatedOutput":"LATE-OK\n","exitCode":0}})),
            note("thread/tokenUsage/updated", "sub", tu(100)),
            note("item/completed", "sub", json!({"item":{"type":"agentMessage","id":"m2","text":"LATE-OK","phase":"final_answer"}})),
            note("thread/tokenUsage/updated", "sub", tu(250)),
            note("turn/completed", "sub", json!({"turn":{"id":"st1","status":"completed"}})),
            note("item/completed", "main", json!({"item":{"type":"subAgentActivity","id":"subagent-completed-st1","kind":"completed","agentThreadId":"sub","agentPath":"/root/late_ok"}})),
        ] {
            handle(&mut st, &sp, &v, &em);
        }
        assert!(tasks().is_empty());

        let evs = evs();
        let bodies: Vec<&EventBody> = evs.iter().map(|e| &e.body).collect();
        assert!(bodies.iter().any(|b| matches!(b, EventBody::ToolCall { id, name, title, .. } if id == "call_1" && name == "spawn_agent" && title == "late_ok")));
        assert!(bodies.iter().any(|b| matches!(b, EventBody::ToolCall { id, .. } if id == "cx-sub:exec-1")));
        assert!(bodies.iter().any(|b| matches!(b, EventBody::ToolResult { id, output, .. } if id == "cx-sub:exec-1" && output == "LATE-OK\n")));
        assert!(bodies.iter().any(|b| matches!(b, EventBody::ToolResult { id, output, is_error: false } if id == "call_1" && output == "LATE-OK")));
        assert!(bodies.iter().any(|b| matches!(b, EventBody::System { text } if text == "后台任务完成：子 agent late_ok")));
        // The subagent's reply is the spawn result, not a main-thread message
        assert_eq!(bodies.iter().filter(|b| matches!(b, EventBody::AssistantText { .. })).count(), 1);
        let usage: u64 = bodies.iter().filter_map(|b| match b { EventBody::Usage { usage } => Some(usage.input_tokens), _ => None }).sum();
        assert_eq!(usage, 250);
        assert_eq!(bodies.iter().filter(|b| matches!(b, EventBody::TurnEnd { .. })).count(), 1);
    }

    #[test]
    fn subagent_stop_task() {
        let (hub, em, _rx) = crate::hub::test_emitter();
        let sp = spec();
        let mut st = State { thread_id: Some("main".into()), ..Default::default() };
        handle(&mut st, &sp, &note("item/started", "main", json!({"item":{"type":"subAgentActivity","id":"call_2","kind":"started","agentThreadId":"sub","agentPath":"/root/sleeper"}})), &em);
        handle(&mut st, &sp, &note("turn/started", "sub", json!({"turn":{"id":"st2"}})), &em);
        assert!(stop_sub(&mut st, "nope").is_none());
        let m = stop_sub(&mut st, "sub").unwrap();
        assert_eq!(m["method"], "turn/interrupt");
        assert_eq!(m["params"], json!({"threadId":"sub","turnId":"st2"}));
        handle(&mut st, &sp, &note("turn/completed", "sub", json!({"turn":{"id":"st2","status":"interrupted"}})), &em);
        let evs = hub.store.events_since("s", 0, 100).unwrap();
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::System { text } if text == "后台任务已停止：子 agent sleeper")));
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::ToolResult { id, is_error: true, .. } if id == "call_2")));
        assert!(hub.info("s").unwrap().background_tasks.is_empty());
        // Never a TurnEnd from a subagent
        assert!(!evs.iter().any(|e| matches!(e.body, EventBody::TurnEnd { .. })));
    }

    #[test]
    fn review_targets() {
        assert_eq!(review_target(""), json!({"type":"uncommittedChanges"}));
        assert_eq!(review_target("branch main"), json!({"type":"baseBranch","branch":"main"}));
        assert_eq!(review_target("commit abc123"), json!({"type":"commit","sha":"abc123"}));
        assert_eq!(review_target("重点看并发安全"), json!({"type":"custom","instructions":"重点看并发安全"}));
        assert_eq!(review_target("branch"), json!({"type":"custom","instructions":"branch"}));
    }
}
