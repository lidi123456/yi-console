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
}

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

pub async fn run(spec: StartSpec, em: Emitter, mut rx: mpsc::UnboundedReceiver<AdapterCmd>) {
    let mut args = vec!["app-server".to_string()];
    args.extend(spec.extra_args.iter().cloned());
    let mut p = match JsonProc::spawn(&spec.program, &args, &spec.cwd) {
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
        json!({"clientInfo":{"name":"yi-console","title":"Yi Console","version":DAEMON_VERSION},
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
                let params = std::mem::take(&mut st.thread_params);
                out.push(st.req(Pending::Thread, "thread/start", params));
                return out;
            }
            em.event(EventBody::Error { message: msg });
            if kind == Pending::Turn {
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
                params["developerInstructions"] = json!(super::IMAGE_HINT);
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
                    for turn in th["turns"].as_array().into_iter().flatten() {
                        for item in turn["items"].as_array().into_iter().flatten() {
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
            for ev in item_events(&params["item"], false) {
                if matches!(ev, EventBody::ToolCall { .. }) {
                    em.event(ev);
                }
            }
        }
        "item/completed" => {
            let item = &params["item"];
            // Commands/patches already emitted a ToolCall at item/started; only the result is needed here, and a patch's diff is taken from completion
            let started_already = matches!(item["type"].as_str(), Some("commandExecution") | Some("mcpToolCall"));
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
            if let Some(t) = params["turn"]["id"].as_str() {
                st.turn_id = Some(t.to_string());
            }
            em.status(SessionStatus::Running, None);
        }
        "turn/completed" => {
            st.turn_id = None;
            let turn = &params["turn"];
            if turn["status"] == "failed" {
                if let Some(m) = turn["error"]["message"].as_str() {
                    em.event(EventBody::Error {
                        message: m.to_string(),
                    });
                }
            }
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
            em.event(EventBody::Plan { entries });
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
            let g = |v: &Value, k: &str| v[k].as_u64().unwrap_or(0);
            let d = |k: &str| {
                g(&total, k).saturating_sub(prev.as_ref().map(|p| g(p, k)).unwrap_or(0))
            };
            // The first report after resume is the historical total; don't count it as usage for this run
            if prev.is_none() && st.resumed {
                return out;
            }
            let cached = d("cachedInputTokens");
            em.event(EventBody::Usage {
                usage: Usage {
                    input_tokens: d("inputTokens").saturating_sub(cached),
                    output_tokens: d("outputTokens"),
                    cache_read_tokens: cached,
                    cache_write_tokens: d("cacheWriteInputTokens"),
                    cost_usd: 0.0,
                    credits: 0.0,
                },
            });
        }
        "error" => {
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
            vec![EventBody::UserMessage { text, images: vec![] }]
        }
        "agentMessage" => {
            let text = s("text");
            if text.trim().is_empty() {
                vec![]
            } else {
                vec![EventBody::AssistantText { id, text }]
            }
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
                    output: crate::util::truncate(&item["result"].to_string(), 64 * 1024),
                    is_error: status == "failed",
                });
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
fn strip_shell_wrapper(cmd: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_targets() {
        assert_eq!(review_target(""), json!({"type":"uncommittedChanges"}));
        assert_eq!(review_target("branch main"), json!({"type":"baseBranch","branch":"main"}));
        assert_eq!(review_target("commit abc123"), json!({"type":"commit","sha":"abc123"}));
        assert_eq!(review_target("重点看并发安全"), json!({"type":"custom","instructions":"重点看并发安全"}));
        assert_eq!(review_target("branch"), json!({"type":"custom","instructions":"branch"}));
    }
}
