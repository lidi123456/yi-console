//! Claude Code adapter: `claude -p --input-format stream-json --output-format stream-json`
//! plus `--permission-prompt-tool stdio`, so permission requests come back over stdin/stdout as control_request(can_use_tool).

use super::{summarize_input, AdapterCmd, JsonProc, Prompt, StartSpec};
use crate::hub::Emitter;
use crate::util::now_ms;
use yi_proto::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::mpsc;

pub fn build_args(spec: &StartSpec) -> Vec<String> {
    let mut a: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-prompt-tool",
        "stdio",
        "--include-partial-messages",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(r) = &spec.resume {
        a.push("--resume".into());
        a.push(r.clone());
    }
    if let Some(m) = spec.model.as_ref().filter(|m| !m.is_empty()) {
        a.push("--model".into());
        a.push(m.clone());
    }
    if let Some(e) = spec.effort.as_ref().filter(|e| !e.is_empty()) {
        a.push("--effort".into());
        a.push(e.clone());
    }
    if let Some(pm) = permission_mode(&spec.permission_mode) {
        a.push("--permission-mode".into());
        a.push(pm.into());
    }
    // Don't add it if the user already set it in extra args, otherwise it's unclear which of two identical flags wins
    if !spec.extra_args.iter().any(|x| x == "--append-system-prompt" || x.starts_with("--append-system-prompt=")) {
        a.push("--append-system-prompt".into());
        a.push(super::IMAGE_HINT.into());
    }
    a.extend(spec.extra_args.iter().cloned());
    a
}

/// Our mode names -> Claude's; default passes no argument.
/// Modes Claude switched to itself that we don't know (e.g. dontAsk) are passed back as is, so they survive a resume
fn permission_mode(m: &str) -> Option<&str> {
    match m {
        "" | "default" => None,
        "bypass" => Some("bypassPermissions"),
        other => Some(other),
    }
}

fn from_claude_mode(m: &str) -> &str {
    match m {
        "bypassPermissions" => "bypass",
        "manual" => "default",
        other => other,
    }
}

/// commands returned by initialize: [{name, description, argumentHint}]
fn parse_commands(r: &Value) -> Vec<SlashCommand> {
    r["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let name = c["name"].as_str()?.trim_start_matches('/').to_string();
            Some(SlashCommand {
                description: crate::util::truncate(c["description"].as_str().unwrap_or(""), 200),
                hint: c["argumentHint"].as_str().unwrap_or("").to_string(),
                name,
            })
        })
        .collect()
}

struct PendingPerm {
    input: Value,
    suggestions: Value,
    /// AskUserQuestion questions; answers are filled in keyed by the question text
    questions: Vec<Question>,
}

#[derive(Default)]
struct State {
    /// Compacting: Claude resends status=compacting about every 30 s while compacting; only record the first
    compacting: bool,
    ready: bool,
    queue: Vec<Prompt>,
    msg_id: String,
    block_n: HashMap<String, u32>,
    perms: HashMap<String, PendingPerm>,
    last_cost: f64,
    req_n: u64,
    /// The user clicked interrupt: this turn's error_during_execution isn't shown as an error
    interrupting: bool,
    /// Inside a turn (between sending a message and the result)
    in_turn: bool,
    /// Background tasks, per the list from background_tasks_changed
    tasks: Vec<BackgroundTask>,
}

pub async fn run(spec: StartSpec, em: Emitter, mut rx: mpsc::UnboundedReceiver<AdapterCmd>) {
    let args = build_args(&spec);
    let mut p = match JsonProc::spawn(&spec.program, &args, &spec.cwd) {
        Ok(p) => p,
        Err(e) => return em.exited(Some(format!("启动 claude 失败: {e}"))),
    };
    let mut st = State::default();
    let _ = p
        .send(&json!({"type":"control_request","request_id":"init","request":{"subtype":"initialize"}}))
        .await;

    let mut killed = false;
    loop {
        tokio::select! {
            line = p.stdout.next_line() => {
                let Ok(Some(line)) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                for out in handle(&mut st, &v, &em) {
                    let _ = p.send(&out).await;
                }
                if st.ready && !st.queue.is_empty() {
                    for prompt in std::mem::take(&mut st.queue) {
                        let _ = p.send(&user_msg(&prompt)).await;
                    }
                }
            }
            cmd = rx.recv() => match cmd {
                Some(AdapterCmd::Send(prompt)) => {
                    st.in_turn = true;
                    em.status(SessionStatus::Running, None);
                    if st.ready { let _ = p.send(&user_msg(&prompt)).await; } else { st.queue.push(prompt); }
                }
                Some(AdapterCmd::SetModel(model)) => {
                    st.req_n += 1;
                    let _ = p.send(&json!({"type":"control_request","request_id":format!("model{}", st.req_n),
                        "request":{"subtype":"set_model","model":model}})).await;
                }
                Some(AdapterCmd::SetEffort(effort)) => {
                    // Change settings while running; the next request uses the new level; null means back to default
                    st.req_n += 1;
                    let lvl = if effort.is_empty() { Value::Null } else { json!(effort) };
                    let _ = p.send(&json!({"type":"control_request","request_id":format!("effort{}", st.req_n),
                        "request":{"subtype":"apply_flag_settings","settings":{"effortLevel":lvl}}})).await;
                }
                Some(AdapterCmd::SetPermissionMode(mode)) => {
                    st.req_n += 1;
                    let m = permission_mode(&mode).unwrap_or("default");
                    let _ = p.send(&json!({"type":"control_request","request_id":format!("pm{}", st.req_n),
                        "request":{"subtype":"set_permission_mode","mode":m}})).await;
                }
                Some(AdapterCmd::StopTask(id)) => {
                    st.req_n += 1;
                    let _ = p.send(&json!({"type":"control_request","request_id":format!("stop{}", st.req_n),
                        "request":{"subtype":"stop_task","task_id":id}})).await;
                }
                Some(AdapterCmd::Interrupt) => {
                    st.interrupting = true;
                    st.req_n += 1;
                    let _ = p.send(&json!({"type":"control_request","request_id":format!("int{}", st.req_n),
                        "request":{"subtype":"interrupt"}})).await;
                }
                Some(AdapterCmd::Permission { request_id, option_id, answers }) => {
                    if let Some(resp) = permission_response(&mut st, &request_id, &option_id, &answers) {
                        let _ = p.send(&resp).await;
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

fn user_msg(p: &Prompt) -> Value {
    let content = if p.images.is_empty() {
        json!(p.text)
    } else {
        let mut blocks: Vec<Value> = p
            .images
            .iter()
            .map(|i| json!({"type":"image","source":{"type":"base64","media_type":i.mime_type,"data":i.data}}))
            .collect();
        if !p.text.trim().is_empty() {
            blocks.push(json!({"type":"text","text":p.text}));
        }
        json!(blocks)
    };
    json!({"type":"user","message":{"role":"user","content":content},"parent_tool_use_id":null,"session_id":""})
}

fn permission_response(st: &mut State, request_id: &str, option_id: &str, answers: &[QuestionAnswer]) -> Option<Value> {
    let pp = st.perms.remove(request_id)?;
    let response = match option_id {
        // AskUserQuestion: fill answers as "question text -> answer", so the model sees them in the tool result
        QUESTION_ANSWER if !pp.questions.is_empty() => {
            let mut map = serde_json::Map::new();
            for q in &pp.questions {
                let a = super::answer_text(q, answers);
                if !a.is_empty() {
                    map.insert(q.question.clone(), json!(a));
                }
            }
            let mut input = pp.input.clone();
            input["answers"] = Value::Object(map);
            json!({"behavior":"allow","updatedInput":input})
        }
        QUESTION_SKIP => json!({"behavior":"deny","message":"用户没有回答这些问题，请按你自己的判断继续，或者直接在回复里说明需要用户确认什么"}),
        "allow_once" => json!({"behavior":"allow","updatedInput":pp.input}),
        "allow_always" => json!({"behavior":"allow","updatedInput":pp.input,
                                  "updatedPermissions": pp.suggestions}),
        _ => json!({"behavior":"deny","message":"用户在 Yi Console 中拒绝了这次操作"}),
    };
    Some(json!({"type":"control_response","response":{
        "subtype":"success","request_id":request_id,"response":response}}))
}

/// Handle one line of output; returns messages to write back to stdin
fn handle(st: &mut State, v: &Value, em: &Emitter) -> Vec<Value> {
    let mut out = vec![];
    match v["type"].as_str().unwrap_or("") {
        "system" => {
            if v["subtype"] == "init" {
                // After a background task ends, Claude starts a turn on its own to handle the result, with no user message
                if !st.in_turn {
                    st.in_turn = true;
                    em.status(SessionStatus::Running, None);
                }
                if let Some(id) = v["session_id"].as_str() {
                    em.external_id(id);
                }
                if let Some(m) = v["model"].as_str() {
                    em.model(m);
                }
            }
            if let Some(t) = system_notice(v) {
                em.event(EventBody::System { text: t });
            }
            match v["subtype"].as_str().unwrap_or("") {
                "background_tasks_changed" => {
                    st.tasks = merge_tasks(&st.tasks, &v["tasks"], now_ms());
                    em.background_tasks(st.tasks.clone());
                }
                "task_notification" => {
                    if let Some(t) = task_notice(&st.tasks, v) {
                        em.event(EventBody::System { text: t });
                    }
                }
                "compact_boundary" => {
                    st.compacting = false;
                    em.event(compact_done(&v["compact_metadata"]))
                }
                // Compaction start and failure are only reported in status (compacting / compact_result)
                "status" if v["status"] == "compacting" => {
                    if !std::mem::replace(&mut st.compacting, true) {
                        em.event(compact_event("start"))
                    }
                }
                "status" if v["compact_result"].as_str().is_some_and(|r| r != "success") => {
                    st.compacting = false;
                    em.event(compact_event("failed"))
                }
                _ => {}
            }
            // Permission mode changed (switched by us, or Claude leaving plan after planning)
            if let Some(m) = v["permissionMode"].as_str() {
                em.permission_mode(from_claude_mode(m));
            }
        }
        "control_response" => {
            let rid = v["response"]["request_id"].as_str().unwrap_or("");
            let r = &v["response"]["response"];
            if rid == "init" && !st.ready {
                st.ready = true;
                em.models(parse_models(r));
                em.commands(parse_commands(r));
                st.req_n += 1;
                out.push(json!({"type":"control_request","request_id":format!("ctx{}", st.req_n),
                    "request":{"subtype":"get_context_usage"}}));
                if st.queue.is_empty() {
                    em.status(SessionStatus::Idle, None);
                }
            } else if rid.starts_with("model") {
                if v["response"]["subtype"] == "error" {
                    let msg = v["response"]["error"].as_str().unwrap_or("切换模型失败");
                    em.event(EventBody::Error { message: format!("切换模型失败: {msg}") });
                } else if let Some(m) = r["model"].as_str() {
                    em.model(m);
                }
            } else if rid.starts_with("effort") && v["response"]["subtype"] == "error" {
                let msg = v["response"]["error"].as_str().unwrap_or("");
                em.event(EventBody::Error { message: format!("切换思考等级失败: {msg}") });
            } else if rid.starts_with("pm") && v["response"]["subtype"] == "error" {
                let msg = v["response"]["error"].as_str().unwrap_or("");
                em.event(EventBody::Error { message: format!("切换审批模式失败: {msg}") });
            } else if rid.starts_with("stop") && v["response"]["subtype"] == "error" {
                let msg = v["response"]["error"].as_str().unwrap_or("");
                em.event(EventBody::Error { message: format!("停止后台任务失败: {msg}") });
            } else if rid.starts_with("ctx") {
                if let Some(p) = r["percentage"].as_f64() {
                    em.context(p);
                }
            }
        }
        "stream_event" => {
            let e = &v["event"];
            match e["type"].as_str().unwrap_or("") {
                "message_start" => {
                    st.msg_id = e["message"]["id"].as_str().unwrap_or("").to_string();
                }
                "content_block_delta" if e["delta"]["type"] == "text_delta" => {
                    if let Some(t) = e["delta"]["text"].as_str() {
                        em.event(EventBody::AssistantDelta {
                            id: st.msg_id.clone(),
                            delta: t.to_string(),
                        });
                    }
                }
                "content_block_delta" if e["delta"]["type"] == "thinking_delta" => {
                    if let Some(t) = e["delta"]["thinking"].as_str() {
                        em.event(EventBody::ThinkingDelta {
                            id: st.msg_id.clone(),
                            delta: t.to_string(),
                        });
                    }
                }
                _ => {}
            }
        }
        "assistant" => {
            // Messages inside a subagent (Task) carry parent_tool_use_id; not expanded for now, to avoid flooding
            if !v["parent_tool_use_id"].is_null() {
                return out;
            }
            let msg = &v["message"];
            let mid = msg["id"].as_str().unwrap_or("").to_string();
            for block in msg["content"].as_array().into_iter().flatten() {
                let n = st.block_n.entry(mid.clone()).or_insert(0);
                *n += 1;
                let bid = format!("{mid}:{n}");
                for ev in blocks_to_events(block, &bid) {
                    em.event(ev);
                }
            }
        }
        "user" => {
            if !v["parent_tool_use_id"].is_null() {
                return out;
            }
            if set_output_files(&mut st.tasks, &v["message"]["content"]) {
                em.background_tasks(st.tasks.clone());
            }
            for ev in user_content_events(&v["message"]["content"]) {
                if !matches!(ev, EventBody::UserMessage { .. }) {
                    em.event(ev);
                }
            }
        }
        "control_request" => {
            let rid = v["request_id"].as_str().unwrap_or("").to_string();
            let req = &v["request"];
            match req["subtype"].as_str().unwrap_or("") {
                "can_use_tool" if req["tool_name"] == "AskUserQuestion" => {
                    let input = req["input"].clone();
                    let questions = ask_questions(&input);
                    st.perms.insert(
                        rid.clone(),
                        PendingPerm {
                            input: input.clone(),
                            suggestions: Value::Null,
                            questions: questions.clone(),
                        },
                    );
                    em.event(EventBody::PermissionRequest {
                        request_id: rid,
                        tool_name: "AskUserQuestion".into(),
                        title: "Claude 有问题想问你".into(),
                        input,
                        options: super::question_options(),
                        questions,
                    });
                }
                "can_use_tool" => {
                    let name = req["tool_name"].as_str().unwrap_or("tool").to_string();
                    let input = req["input"].clone();
                    let suggestions = req["permission_suggestions"].clone();
                    let mut options = vec![PermissionOption {
                        id: "allow_once".into(),
                        label: "允许".into(),
                        kind: "allow_once".into(),
                    }];
                    if suggestions.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                        options.push(PermissionOption {
                            id: "allow_always".into(),
                            label: "始终允许".into(),
                            kind: "allow_always".into(),
                        });
                    }
                    options.push(PermissionOption {
                        id: "reject_once".into(),
                        label: "拒绝".into(),
                        kind: "reject_once".into(),
                    });
                    let title = req["description"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(|d| format!("{name}: {d}"))
                        .unwrap_or_else(|| format!("{name}: {}", summarize_input(&name, &input)));
                    st.perms.insert(
                        rid.clone(),
                        PendingPerm {
                            input: input.clone(),
                            suggestions,
                            questions: vec![],
                        },
                    );
                    em.event(EventBody::PermissionRequest {
                        request_id: rid,
                        tool_name: name,
                        title,
                        input,
                        options,
                        questions: vec![],
                    });
                }
                _ => {
                    // Capabilities we didn't register (hook_callback / mcp_message, etc.): reply with an empty success
                    out.push(json!({"type":"control_response","response":{
                        "subtype":"success","request_id":rid,"response":{}}}));
                }
            }
        }
        "result" => {
            st.in_turn = false;
            let u = &v["usage"];
            let total = v["total_cost_usd"].as_f64().unwrap_or(0.0);
            // total_cost_usd is cumulative within the process; convert to this turn's increment
            let cost = if total >= st.last_cost {
                total - st.last_cost
            } else {
                total
            };
            st.last_cost = total;
            em.event(EventBody::Usage {
                usage: Usage {
                    input_tokens: u["input_tokens"].as_u64().unwrap_or(0),
                    output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                    cache_read_tokens: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                    cache_write_tokens: u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
                    cost_usd: cost,
                    credits: 0.0,
                },
            });
            let interrupted = std::mem::take(&mut st.interrupting) && v["subtype"] == "error_during_execution";
            if v["is_error"].as_bool().unwrap_or(false) && !interrupted {
                let msg = v["result"]
                    .as_str()
                    .or(v["subtype"].as_str())
                    .unwrap_or("执行出错")
                    .to_string();
                em.event(EventBody::Error { message: msg });
            }
            em.event(EventBody::TurnEnd {
                stop_reason: if interrupted {
                    Some("interrupted".into())
                } else {
                    v["stop_reason"].as_str().map(str::to_string)
                },
                model: None,
            });
            // Ask for context usage after the turn ends
            st.req_n += 1;
            out.push(json!({"type":"control_request","request_id":format!("ctx{}", st.req_n),
                "request":{"subtype":"get_context_usage"}}));
        }
        _ => {}
    }
    out
}

/// models in the initialize response
pub fn parse_models(r: &Value) -> Vec<ModelOption> {
    r["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(ModelOption {
                id: m["value"].as_str()?.to_string(),
                name: m["displayName"].as_str().unwrap_or("").to_string(),
                description: m["description"].as_str().unwrap_or("").to_string(),
                resolved: m["resolvedModel"].as_str().unwrap_or("").to_string(),
                efforts: if m["supportsEffort"].as_bool().unwrap_or(false) {
                    strings(&m["supportedEffortLevels"])
                } else {
                    vec![]
                },
                default_effort: String::new(),
            })
        })
        .collect()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(String::from))
        .collect()
}

/// Blank or only zero-width characters (models occasionally emit a long run, which shows as an empty message)
fn is_blank(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_whitespace() || matches!(c, '\u{200b}'..='\u{200d}' | '\u{2060}' | '\u{feff}'))
}

/// Claude Code's own notices: retrying after a reply was interrupted by safety mechanisms, retrying failed requests, etc.
/// Without showing them, the UI just spins forever
fn system_notice(v: &Value) -> Option<String> {
    match v["subtype"].as_str()? {
        "informational" => {
            let c = v["content"].as_str()?.trim();
            if c.is_empty() {
                None
            } else if c.contains("safeguards stopped") {
                Some("上一段回复被模型的安全机制中断，Claude 正在自动重试".to_string())
            } else {
                Some(c.to_string())
            }
        }
        "api_retry" => {
            let attempt = v["attempt"].as_u64().unwrap_or(0);
            let status = v["error_status"].as_u64().map(|s| format!("（HTTP {s}）")).unwrap_or_default();
            let wait = v["retry_delay_ms"].as_u64().map(|ms| format!("，{} 秒后重试", (ms + 999) / 1000)).unwrap_or_default();
            Some(format!("请求模型失败{status}{wait}，第 {attempt} 次重试"))
        }
        _ => None,
    }
}

fn compact_event(phase: &str) -> EventBody {
    EventBody::Compact { phase: phase.into(), trigger: None, pre_tokens: None, post_tokens: None, duration_ms: None }
}

/// compact_metadata of compact_boundary: trigger, tokens before/after, duration
fn compact_done(m: &Value) -> EventBody {
    EventBody::Compact {
        phase: "done".into(),
        trigger: m["trigger"].as_str().map(String::from),
        pre_tokens: m["pre_tokens"].as_u64(),
        post_tokens: m["post_tokens"].as_u64(),
        duration_ms: m["duration_ms"].as_u64(),
    }
}

/// tasks in background_tasks_changed are all current background tasks; existing ones keep their discovery time and output file
fn merge_tasks(old: &[BackgroundTask], tasks: &Value, now: i64) -> Vec<BackgroundTask> {
    tasks
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let id = t["task_id"].as_str()?;
            let prev = old.iter().find(|o| o.id == id);
            Some(BackgroundTask {
                id: id.to_string(),
                kind: t["task_type"].as_str().unwrap_or("").to_string(),
                description: t["description"].as_str().unwrap_or("").to_string(),
                started_at: prev.map(|p| p.started_at).unwrap_or(now),
                output_file: prev.and_then(|p| p.output_file.clone()),
            })
        })
        .collect()
}

/// The tool_result of a background command names the output file:
/// "Command running in background with ID: xxx. Output is being written to: /path/xxx.output. ..."
fn set_output_files(tasks: &mut [BackgroundTask], content: &Value) -> bool {
    let mut changed = false;
    for item in content.as_array().into_iter().flatten() {
        let text = match &item["content"] {
            Value::String(s) => s.clone(),
            Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
            _ => continue,
        };
        let Some(rest) = text.split("Output is being written to: ").nth(1) else {
            continue;
        };
        let Some(end) = rest.find(".output") else {
            continue;
        };
        let path = &rest[..end + ".output".len()];
        for t in tasks.iter_mut() {
            if t.output_file.is_none() && text.contains(&format!("ID: {}.", t.id)) {
                t.output_file = Some(path.to_string());
                changed = true;
            }
        }
    }
    changed
}

/// Leave a line in the chat when a background task ends
fn task_notice(tasks: &[BackgroundTask], v: &Value) -> Option<String> {
    let id = v["task_id"].as_str()?;
    let what = tasks
        .iter()
        .find(|t| t.id == id)
        .map(|t| t.description.clone())
        .filter(|d| !d.is_empty())
        .or_else(|| v["summary"].as_str().map(str::to_string))
        .unwrap_or_else(|| id.to_string());
    let status = match v["status"].as_str().unwrap_or("") {
        "completed" => "后台任务完成",
        "failed" => "后台任务失败",
        "stopped" | "killed" => "后台任务已停止",
        other => return Some(format!("后台任务 {other}：{what}")),
    };
    Some(format!("{status}：{what}"))
}

/// One content block in an assistant message -> unified events (also reused by history import)
pub fn blocks_to_events(block: &Value, bid: &str) -> Vec<EventBody> {
    match block["type"].as_str().unwrap_or("") {
        "text" => {
            let text = block["text"].as_str().unwrap_or("").to_string();
            if is_blank(&text) {
                vec![]
            } else {
                vec![EventBody::AssistantText {
                    id: bid.to_string(),
                    text,
                }]
            }
        }
        "thinking" => {
            let text = block["thinking"].as_str().unwrap_or("").to_string();
            if text.trim().is_empty() {
                vec![]
            } else {
                vec![EventBody::Thinking {
                    id: bid.to_string(),
                    text,
                }]
            }
        }
        "tool_use" => {
            let name = block["name"].as_str().unwrap_or("tool").to_string();
            let input = block["input"].clone();
            let id = block["id"].as_str().unwrap_or(bid).to_string();
            let mut evs = vec![EventBody::ToolCall {
                id,
                title: summarize_input(&name, &input),
                diffs: tool_diffs(&name, &input),
                name: name.clone(),
                input: input.clone(),
            }];
            if name == "TodoWrite" {
                let entries = input["todos"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|t| PlanEntry {
                        content: t["content"].as_str().unwrap_or("").to_string(),
                        status: t["status"].as_str().unwrap_or("pending").to_string(),
                    })
                    .collect();
                evs.push(EventBody::Plan { entries });
            }
            evs
        }
        _ => vec![],
    }
}

/// User message content -> user text / tool results
pub fn user_content_events(content: &Value) -> Vec<EventBody> {
    if let Some(s) = content.as_str() {
        return vec![EventBody::UserMessage {
            text: s.to_string(),
            images: vec![],
        }];
    }
    let mut out = vec![];
    for b in content.as_array().into_iter().flatten() {
        match b["type"].as_str().unwrap_or("") {
            "tool_result" => out.push(EventBody::ToolResult {
                id: b["tool_use_id"].as_str().unwrap_or("").to_string(),
                output: tool_result_text(&b["content"]),
                is_error: b["is_error"].as_bool().unwrap_or(false),
            }),
            "text" => out.push(EventBody::UserMessage {
                text: b["text"].as_str().unwrap_or("").to_string(),
                images: vec![],
            }),
            _ => {}
        }
    }
    out
}

fn tool_result_text(c: &Value) -> String {
    let s = if let Some(s) = c.as_str() {
        s.to_string()
    } else {
        c.as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    crate::util::truncate(&s, 64 * 1024)
}

fn tool_diffs(name: &str, input: &Value) -> Vec<FileDiff> {
    let path = input["file_path"].as_str().unwrap_or("").to_string();
    let s = |v: &Value| v.as_str().map(str::to_string);
    match name {
        "Edit" => vec![FileDiff {
            path,
            old_text: s(&input["old_string"]),
            new_text: s(&input["new_string"]),
            unified: None,
        }],
        "MultiEdit" => input["edits"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| FileDiff {
                path: path.clone(),
                old_text: s(&e["old_string"]),
                new_text: s(&e["new_string"]),
                unified: None,
            })
            .collect(),
        "Write" => vec![FileDiff {
            path,
            old_text: None,
            new_text: s(&input["content"]),
            unified: None,
        }],
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_tasks() {
        // Format observed from real stream-json
        let changed = json!([{"task_id":"bheoe6moo","run_id":"r","task_type":"local_bash","description":"Sleep 4 seconds"}]);
        let mut tasks = merge_tasks(&[], &changed, 100);
        assert_eq!(tasks.len(), 1);
        assert_eq!((tasks[0].kind.as_str(), tasks[0].started_at), ("local_bash", 100));

        let content = json!([{"tool_use_id":"toolu_1","type":"tool_result","content":"Command running in background with ID: bheoe6moo. Output is being written to: /tmp/claude-0/x/tasks/bheoe6moo.output. You will be notified when it completes.","is_error":false}]);
        assert!(set_output_files(&mut tasks, &content));
        assert_eq!(tasks[0].output_file.as_deref(), Some("/tmp/claude-0/x/tasks/bheoe6moo.output"));
        assert!(!set_output_files(&mut tasks, &content));

        // Another list change: discovery time and output file are kept
        let again = merge_tasks(&tasks, &changed, 999);
        assert_eq!(again[0].started_at, 100);
        assert!(again[0].output_file.is_some());
        assert!(merge_tasks(&again, &json!([]), 999).is_empty());

        let n = json!({"type":"system","subtype":"task_notification","task_id":"bheoe6moo","status":"stopped","summary":"x"});
        assert_eq!(task_notice(&tasks, &n).as_deref(), Some("后台任务已停止：Sleep 4 seconds"));
    }

    #[test]
    fn invisible_text_and_notices() {
        assert!(is_blank("\u{200b}\u{200c}\u{200d} \n"));
        assert!(!is_blank("\u{200b}好"));
        assert!(blocks_to_events(&json!({"type":"text","text":"\u{200b}\u{200c}"}), "m:1").is_empty());
        let v = json!({"type":"system","subtype":"informational",
            "content":"Opus 5.5's safeguards stopped the response above · continuing once with that noted"});
        assert!(system_notice(&v).unwrap().contains("安全机制"));
        let v = json!({"type":"system","subtype":"api_retry","attempt":2,"retry_delay_ms":1500,"error_status":529});
        assert_eq!(system_notice(&v).unwrap(), "请求模型失败（HTTP 529），2 秒后重试，第 2 次重试");
        assert!(system_notice(&json!({"type":"system","subtype":"status","status":"requesting"})).is_none());
    }
}

/// AskUserQuestion input: {questions: [{question, header, options: [{label, description}], multiSelect}]}.
/// Claude's UI lets every question pick "Other" and type an answer, so allow that here too
fn ask_questions(input: &Value) -> Vec<Question> {
    input["questions"]
        .as_array()
        .map(|a| {
            a.iter()
                .enumerate()
                .map(|(i, q)| Question {
                    id: i.to_string(),
                    header: q["header"].as_str().unwrap_or("").to_string(),
                    question: q["question"].as_str().unwrap_or("").to_string(),
                    options: q["options"]
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
                        .unwrap_or_default(),
                    multi_select: q["multiSelect"].as_bool().unwrap_or(false),
                    allow_other: true,
                    secret: false,
                })
                .collect()
        })
        .unwrap_or_default()
}
