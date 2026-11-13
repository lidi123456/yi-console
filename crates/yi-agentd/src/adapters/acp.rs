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
    /// Antigravity's `thought_level` option
    SetEffort,
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
    /// The session's appended system prompt still has to go out with the next prompt (new tool session or changed)
    system_notice: Option<String>,
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
    /// The `session/set_config_option` waiting for its answer
    config_inflight: Option<(String, String)>,
    /// Parameters the user chose for the current model; sent again after a failed model switch
    wanted: Vec<(String, String)>,
    /// A failed model switch was already retried once
    model_retried: bool,
    /// The model switch failed twice; tried again before the next prompt
    model_failed: Option<String>,
    /// The model was switched before the session existed, so the parameters saved at launch belong to the old one
    spec_params_stale: bool,
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
    /// The tool has an ACP mode for every permission mode (Antigravity), so each one is set with session/set_mode
    native_modes: bool,
    /// The tool's current ACP mode id, when `native_modes`
    native_mode: Option<String>,
    turn_credits: f64,
    /// Antigravity: the thinking level chosen in Yi Console, kept on the tool's `thought_level` option. agy resets the
    /// option to its default on every model switch, so it is set again whenever the tool reports a different value
    effort: Option<String>,
    /// Levels the tool offers for the current model, and the one it is on
    thought_levels: Vec<String>,
    thought_current: Option<String>,
    /// Text of the turn's last reply (Antigravity reports a used-up quota only as reply text)
    last_reply: String,
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
    /// Antigravity (`agy acp`): tracks asynchronous subagents spawned in the current turn
    is_antigravity: bool,
    agy_pre_turn_steps: usize,
    agy_pending_calls: HashMap<String, AgyPendingCall>,
    agy_send_baselines: HashMap<String, usize>,
    agy_watch: Option<AgyWatch>,
    /// Subagents still running when a new prompt went out: the prompt's own watch picks them up when it returns
    agy_carry: AgyCarry,
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
    // Allow-all: agy never sends its subagents' approval requests over ACP, so only this flag lets them run commands
    if spec.tool == ToolKind::Antigravity && spec.permission_mode == "bypass" {
        a.push("--dangerously-skip-permissions".into());
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
        native_modes: spec.tool == ToolKind::Antigravity,
        is_antigravity: spec.tool == ToolKind::Antigravity,
        effort: spec.effort.clone().filter(|e| !e.is_empty() && spec.tool == ToolKind::Antigravity),
        desktop_notice: spec.mcp_servers.iter().any(|server| server["name"] == crate::mcp::DESKTOP_SERVER),
        system_notice: super::acp_system_notice(&spec),
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
    let mut agy_tick = tokio::time::interval(std::time::Duration::from_millis(250));
    agy_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
                if let Some(m) = next_effort(&mut st) { let _ = p.send(&m).await; }
                if st.session_id.is_some() && !config_pending(&st) && !st.prompting && !st.loading && st.compacting.is_none() && !st.queue.is_empty() {
                    let next = st.queue.remove(0);
                    let m = prompt(&mut st, &em, &next);
                    let _ = p.send(&m).await;
                }
            }
            _ = agy_tick.tick(), if st.agy_watch.is_some() => {
                if let Some(m) = poll_agy_watch(&mut st, &spec, &em) {
                    let _ = p.send(&m).await;
                }
                if !st.prompting && st.session_id.is_some() && !config_pending(&st) && !st.loading && st.compacting.is_none() && !st.queue.is_empty() {
                    let next = st.queue.remove(0);
                    let m = prompt(&mut st, &em, &next);
                    let _ = p.send(&m).await;
                }
            }
            cmd = rx.recv() => match cmd {
                Some(AdapterCmd::Send(next)) => {
                    em.status(SessionStatus::Running, None);
                    if retry_failed_model(&mut st) {
                        if let Some(m) = next_config(&mut st) { let _ = p.send(&m).await; }
                    }
                    if st.session_id.is_some() && !config_pending(&st) && !st.prompting && !st.loading && st.compacting.is_none() {
                        let m = prompt(&mut st, &em, &next);
                        let _ = p.send(&m).await;
                    } else {
                        st.queue.push(next);
                    }
                }
                Some(AdapterCmd::SetModel(model)) => {
                    // Parameters chosen for the previous model don't carry over
                    st.wanted.clear();
                    st.config_queue.clear();
                    st.model_failed = None;
                    st.model_retried = false;
                    if st.session_id.is_some() {
                        if st.parameterized {
                            st.config_queue.push(("model".into(), base_model(&model).into()));
                            if let Some(m) = next_config(&mut st) { let _ = p.send(&m).await; }
                        } else {
                            let m = set_model(&mut st, &model);
                            let _ = p.send(&m).await;
                        }
                    } else {
                        st.want_model = Some(model);
                        st.spec_params_stale = true;
                    }
                }
                Some(AdapterCmd::SetModelConfig { id, value }) => {
                    st.wanted.retain(|(k, _)| *k != id);
                    st.wanted.push((id.clone(), value.clone()));
                    st.config_queue.push((id, value));
                    if let Some(m) = next_config(&mut st) { let _ = p.send(&m).await; }
                }
                Some(AdapterCmd::SetEffort(effort)) if spec.tool == ToolKind::Antigravity => {
                    // Back to "default" is agy's own default level
                    st.effort = Some(if effort.is_empty() { "medium".into() } else { effort });
                    if let Some(m) = next_effort(&mut st) { let _ = p.send(&m).await; }
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
                    // The watch's turn is only open while the parent's wakeup runs. Interrupting that stops the parent;
                    // subagents still running stay background tasks (stopped one by one from the list)
                    if st.agy_watch.as_ref().is_some_and(|w| w.reopened) {
                        carry_agy_watch(&mut st, &em);
                        st.prompting = false;
                        st.end_subagents();
                        em.event(EventBody::TurnEnd { stop_reason: Some("cancelled".into()), model: None });
                        if !st.agy_carry.subs.is_empty() {
                            if let Some(sid) = st.session_id.clone() {
                                let steps = count_agy_transcript_steps(&agy_brain_dir(), &sid);
                                let mut w = AgyWatch::new(sid, agy_brain_dir(), steps, steps, None, HashMap::new(), HashMap::new(), st.perm != "bypass");
                                w.adopt(std::mem::take(&mut st.agy_carry));
                                w.detached = true;
                                st.agy_watch = Some(w);
                            }
                        }
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
                Some(AdapterCmd::StopTask(task)) if st.is_antigravity => {
                    // Background task ids are subagent conversation ids
                    if let Some(m) = stop_agy_subagent(&mut st, &em, &task) {
                        let _ = p.send(&m).await;
                    }
                }
                Some(AdapterCmd::Kill) | None => { killed = true; break; }
                Some(_) => {}
            }
        }
    }
    drop_agy_watch(&mut st, &em);
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
    !st.config_queue.is_empty() || st.pending.values().any(|p| matches!(p, Pending::SetModel | Pending::SetConfig | Pending::SetEffort))
}

/// Antigravity: record the `thought_level` option from a configOptions list (absent: the model has no levels)
fn read_thought_level(st: &mut State, v: &Value) {
    let Some(options) = v["configOptions"].as_array() else { return };
    let option = options.iter().find(|o| o["category"] == "thought_level");
    st.thought_levels = option.and_then(|o| o["options"].as_array()).into_iter().flatten()
        .filter_map(|o| o["value"].as_str().map(str::to_string)).collect();
    st.thought_current = option.and_then(|o| o["currentValue"].as_str()).map(str::to_string);
}

/// Antigravity: set the chosen thinking level when the tool is on another one and the current model offers it
fn next_effort(st: &mut State) -> Option<Value> {
    let want = st.effort.clone()?;
    if st.loading || st.session_id.is_none() || st.pending.values().any(|p| matches!(p, Pending::SetModel | Pending::SetEffort))
        || st.thought_current.as_deref() == Some(want.as_str()) || !st.thought_levels.contains(&want) {
        return None;
    }
    st.thought_current = Some(want.clone());
    Some(st.req(Pending::SetEffort, "session/set_config_option",
        json!({"sessionId":st.session_id,"configId":"thought_level","value":want})))
}

/// `grok-4.7[context=256k]` (the old single-string form) -> `grok-4.7`
fn base_model(model: &str) -> &str {
    model.split('[').next().unwrap_or(model)
}

/// Before a prompt: a model switch that failed is sent again with the chosen parameters
fn retry_failed_model(st: &mut State) -> bool {
    let Some(model) = st.model_failed.take() else { return false };
    st.model_retried = false;
    st.config_queue.insert(0, ("model".into(), model));
    let wanted = st.wanted.clone();
    st.config_queue.extend(wanted);
    true
}

fn next_config(st: &mut State) -> Option<Value> {
    if st.loading || st.session_id.is_none() || !st.parameterized || st.pending.values().any(|p| matches!(p, Pending::SetModel | Pending::SetConfig)) {
        return None;
    }
    while !st.config_queue.is_empty() {
        let (mut id, value) = st.config_queue.remove(0);
        // The old model string used `reasoning_effort` for every model; Cursor now names it per model
        if id != "model" && !st.config_options.iter().any(|o| o.id == id) && matches!(id.as_str(), "reasoning" | "reasoning_effort" | "effort") {
            if let Some(o) = st.config_options.iter().find(|o| o.category == "thought_level") { id = o.id.clone(); }
        }
        // Saved parameters may no longer exist after a model switch or tool upgrade.
        if id != "model" && !st.config_options.iter().any(|o| o.id == id && o.options.iter().any(|v| v.value == value)) { continue; }
        st.config_inflight = Some((id.clone(), value.clone()));
        return Some(st.req(Pending::SetConfig, "session/set_config_option",
            json!({"sessionId":st.session_id,"configId":id,"value":value})));
    }
    None
}

/// The current model and its adjustable parameters in a `configOptions` list
pub(crate) fn config_params(r: &Value) -> Option<(Option<String>, Vec<ModelConfigOption>)> {
    let options = r["configOptions"].as_array()?;
    let model = options.iter().find(|o| o["id"] == "model").and_then(|o| o["currentValue"].as_str()).map(str::to_string);
    let params = options.iter().filter(|o| matches!(o["category"].as_str(), Some("model_config" | "thought_level")))
        .filter_map(|o| serde_json::from_value::<ModelConfigOption>(o.clone()).ok()).collect();
    Some((model, params))
}

/// Track the tool's state. Only the answer to `session/set_config_option` is published: Cursor's `session/new`
/// lists `mode` and `model` but not the model's parameters, which would read as "this model has none"
fn apply_config_options(st: &mut State, r: &Value) {
    let Some((model, params)) = config_params(r) else { return; };
    if model.is_some() { st.config_model = model; }
    st.config_options = params;
}

fn publish_config(st: &State, em: &Emitter) {
    if let Some(model) = &st.config_model {
        em.model_config_confirmed(model, st.config_options.clone());
    }
}

fn prompt(st: &mut State, em: &Emitter, p: &Prompt) -> Value {
    st.prompting = true;
    st.last_reply.clear();
    st.compact_manual = p.text.trim().starts_with("/compact");
    st.turn_credits = 0.0;
    st.agy_pending_calls.clear();
    st.agy_send_baselines.clear();
    // The watch can't run during a prompt (the parent's own steps would look like a wakeup): its running subagents
    // wait in agy_carry for this prompt's watch. A report arriving meanwhile is handed to this turn by agy itself
    carry_agy_watch(st, em);
    st.agy_pre_turn_steps = match st.session_id.as_deref() {
        Some(sid) if st.is_antigravity => count_agy_transcript_steps(&agy_brain_dir(), sid),
        _ => 0,
    };
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
    if !p.text.trim_start().starts_with('/') {
        if let Some(notice) = st.system_notice.take() {
            blocks.push(json!({"type":"text","text":notice}));
        }
    }
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
    if st.native_modes {
        let target = antigravity_mode(&st.perm);
        if st.native_mode.as_deref() == Some(target) {
            return None;
        }
        st.native_mode = Some(target.to_string());
        return Some(st.req(Pending::Other, "session/set_mode", json!({"sessionId":sid,"modeId":target})));
    }
    let target = if st.perm == "plan" {
        st.plan_mode.clone()?
    } else if was_plan {
        st.base_mode.clone()?
    } else {
        return None;
    };
    Some(st.req(Pending::Other, "session/set_mode", json!({"sessionId":sid,"modeId":target})))
}

/// Antigravity's ACP mode for a permission mode. Allow-all stays in default: every request it asks about is answered here
fn antigravity_mode(perm: &str) -> &'static str {
    match perm {
        "acceptEdits" => "accept-edits",
        "auto" => "auto",
        "plan" => "plan",
        _ => "default",
    }
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
    if seg == Seg::Agent && !st.loading {
        st.last_reply = text.clone();
    }
    em.event(match seg {
        Seg::Agent => EventBody::AssistantText { id, text },
        Seg::Thought => EventBody::Thinking { id, text },
        Seg::User => EventBody::UserMessage { text, images: vec![], origin: None },
    });
}

fn chunk(st: &mut State, em: &Emitter, seg: Seg, text: &str) {
    // Antigravity echoes the prompt as user_message_chunk in a live turn too; the hub already recorded it
    if seg == Seg::User && !st.loading {
        return;
    }
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
                    if spec.tool == ToolKind::Cursor { em.model_config_settled(); }
                    em.event(EventBody::Error {
                        message: format!("切换模型失败: {msg}"),
                    });
                    Ok(out)
                }
                Pending::SetEffort => {
                    // Don't try again on every option update
                    st.effort = None;
                    em.event(EventBody::Error { message: format!("设置思考等级失败: {msg}") });
                    Ok(out)
                }
                Pending::SetConfig => {
                    let (id, value) = st.config_inflight.take().unwrap_or_default();
                    if id == "model" {
                        // Cursor asks its server for the model list on every switch and reports a failed request as
                        // "Invalid model value", so one retry usually goes through
                        if !st.model_retried {
                            st.model_retried = true;
                            st.config_queue.insert(0, (id, value));
                            return Ok(out);
                        }
                        st.model_retried = false;
                        // The queued parameters were for that model; they go again with the next attempt
                        st.config_queue.retain(|(k, _)| k == "model");
                        st.model_failed = Some(value);
                        em.model_config_settled();
                        em.event(EventBody::Error { message: format!("切换模型失败，发下一条消息时会再试: {msg}") });
                    } else {
                        st.wanted.retain(|(k, _)| *k != id);
                        let actual = st.config_options.iter().find(|o| o.id == id).map(|o| o.current_value.clone()).unwrap_or_default();
                        em.model_config_value(&id, &actual);
                        em.event(EventBody::Error { message: format!("切换模型参数失败: {msg}") });
                    }
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
                // Antigravity's first method is a browser sign-in on the host; leave it to `agy acp --login`
                st.auth_method = r["authMethods"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|m| m["id"].as_str())
                    .filter(|_| spec.tool != ToolKind::Antigravity)
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
                let agy = spec.tool == ToolKind::Antigravity;
                let models = if agy { antigravity_models(r) } else { parse_models(r) };
                if agy { read_thought_level(st, r); }
                st.parameterized = spec.tool == ToolKind::Cursor && r["configOptions"].is_array();
                if st.parameterized { apply_config_options(st, r); }
                else if spec.tool == ToolKind::Cursor { em.model_config_settled(); }
                // Antigravity's currentModelId is a model+level variant; its `model` option holds the model itself
                let current = if agy { config_params(r).and_then(|(m, _)| m) } else { None }
                    .or_else(|| r["models"]["currentModelId"].as_str().map(str::to_string));
                // If the model recorded on the session (picked at creation or switched later) differs from the tool's current one, switch to it
                let queued_config = std::mem::take(&mut st.config_queue);
                let want = st
                    .want_model
                    .take()
                    .or(spec.model.clone())
                    .map(|m| if st.parameterized { base_model(&m).to_string() } else { m })
                    .filter(|m| !m.is_empty());
                let listed = |m: &str| models.len() <= 1 || models.iter().any(|o| o.id == m);
                if st.parameterized {
                    let use_spec = !st.spec_params_stale && want.as_deref().is_some_and(|w| listed(w));
                    // Always switch, even to the model Cursor is already on: only that answer lists the model's parameters
                    let target = match (&want, &current) {
                        (Some(w), _) if listed(w) => Some(w.clone()),
                        (Some(w), Some(c)) => {
                            em.event(EventBody::Error { message: format!("Cursor 没有模型 {w}，改用 {c}") });
                            em.model(c);
                            Some(c.clone())
                        }
                        (None, Some(c)) => {
                            em.model(c);
                            Some(c.clone())
                        }
                        _ => None,
                    };
                    match target {
                        Some(t) => st.config_queue.insert(0, ("model".into(), t)),
                        None => em.model_config_settled(),
                    }
                    if use_spec {
                        // Old sessions saved the parameters in the model string; `reasoning_effort` was later split into per-model ids
                        if let Some(params) = spec.model.as_deref().and_then(|m| m.split_once('[')).map(|(_, p)| p.trim_end_matches(']')) {
                            for param in params.split(',') {
                                if let Some((id, value)) = param.split_once('=') {
                                    st.wanted.push((id.trim().into(), value.trim().into()));
                                }
                            }
                        }
                        // Restore after the model switch, before any queued user prompt.
                        for o in spec.model_config_options.iter().filter(|o| !o.current_value.is_empty()) {
                            st.wanted.retain(|(k, _)| *k != o.id);
                            st.wanted.push((o.id.clone(), o.current_value.clone()));
                        }
                        let wanted = st.wanted.clone();
                        st.config_queue.extend(wanted);
                    }
                } else {
                    match (want.filter(|w| listed(w)), &current) {
                        (Some(w), c) if c.as_deref() != Some(w.as_str()) => out.push(set_model(st, &w)),
                        (_, Some(c)) => em.model(c),
                        _ => {}
                    }
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
                if st.native_modes {
                    st.native_mode = r["modes"]["currentModeId"].as_str().map(str::to_string);
                }
                if st.perm == "plan" || st.native_modes {
                    if let Some(m) = apply_mode(st, false) {
                        out.push(m);
                    }
                }
                if st.queue.is_empty() {
                    em.status(SessionStatus::Idle, None);
                }
            }
            Pending::Prompt => {
                flush(st, em);
                let meta_err = meta_error(r);
                if let Some(msg) = meta_err.as_ref() {
                    em.event(EventBody::Error { message: login_hint(spec, msg) });
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
                // Antigravity reports the turn's tokens in the prompt result (and keeps no usage log of its own)
                if spec.tool == ToolKind::Antigravity {
                    if let Some(usage) = prompt_usage(&r["usage"]) {
                        em.event(EventBody::Usage { usage });
                    }
                }
                let ctx = match spec.tool {
                    ToolKind::Cursor => st.session_id.as_deref().and_then(cursor_context),
                    ToolKind::Qoder => st.session_id.as_deref().and_then(qoder_context),
                    _ => None,
                };
                if let Some(p) = ctx {
                    em.context(p);
                }
                let limit_left = (spec.tool == ToolKind::Antigravity).then(|| antigravity_limit(&st.last_reply)).flatten();
                if let Some(left) = limit_left {
                    // Which window ran out isn't said; one resetting within 5 hours is the 5-hour one
                    let limit = if left <= 5 * 3_600_000 { "five_hour" } else { "seven_day" };
                    em.event(super::limit_hit(Some(crate::util::now_ms() + left), Some(limit.into())));
                }
                let stop_reason = r["stopReason"].as_str().map(str::to_string);
                let should_watch = st.is_antigravity
                    && (!st.agy_carry.subs.is_empty()
                        || (meta_err.is_none()
                            && limit_left.is_none()
                            && stop_reason.as_deref() != Some("cancelled")
                            && (!st.agy_pending_calls.is_empty()
                                || st.session_id.as_deref().is_some_and(|sid| {
                                    has_pending_agy_subagent_turn(&agy_brain_dir(), sid, st.agy_pre_turn_steps)
                                }))));
                if should_watch {
                    if let Some(sid) = st.session_id.clone() {
                        let wake_from = count_agy_transcript_steps(&agy_brain_dir(), &sid);
                        let mut watch = AgyWatch::new(
                            sid,
                            agy_brain_dir(),
                            st.agy_pre_turn_steps,
                            wake_from,
                            stop_reason,
                            std::mem::take(&mut st.agy_pending_calls),
                            std::mem::take(&mut st.agy_send_baselines),
                            st.perm != "bypass",
                        );
                        watch.adopt(std::mem::take(&mut st.agy_carry));
                        // The turn ends now; the subagents go on as background tasks
                        watch.detached = true;
                        // One pass right away, so subagents that already finished are picked up without waiting for a tick
                        let step = step_agy_watch(&mut watch, spec, em, &mut st.next_id, &mut st.pending);
                        st.prompting = false;
                        st.end_subagents();
                        // A wakeup already seen in this pass reopened the turn: that one ends it
                        let reopened = watch.reopened;
                        if !reopened {
                            em.event(EventBody::TurnEnd { stop_reason: watch.stop_reason.clone(), model: None });
                        }
                        match step {
                            AgyStep::Done(req) => {
                                out.extend(req);
                                if reopened {
                                    em.event(EventBody::TurnEnd { stop_reason: watch.stop_reason, model: None });
                                }
                            }
                            AgyStep::Wait => st.agy_watch = Some(watch),
                        }
                        return Ok(out);
                    }
                }
                for (id, _) in st.agy_pending_calls.drain() {
                    em.event(EventBody::ToolResult {
                        id,
                        output: String::new(),
                        is_error: false,
                    });
                }
                st.prompting = false;
                st.end_subagents();
                em.event(EventBody::TurnEnd {
                    stop_reason,
                    model: None,
                });
            }
            Pending::SetModel => {
                if let Some(m) = st.switching.take() {
                    em.model(&m);
                }
                if spec.tool == ToolKind::Cursor { em.model_config_settled(); }
            }
            Pending::SetConfig => {
                if st.config_inflight.take().is_some_and(|(id, _)| id == "model") { st.model_retried = false; }
                apply_config_options(st, r);
                publish_config(st, em);
            }
            Pending::SetEffort => read_thought_level(st, r),
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
            // Antigravity asks the user questions through request_permission with an `interaction_` id; the options are answers
            let question = tc_id.starts_with("interaction_");
            if !question && (st.perm == "bypass" || (st.perm == "acceptEdits" && is_edit)) {
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
/// Cursor fills `availableModels` from a server request of its own, and when that fails lists none or only the
/// current model; the values of its `model` option are complete, so the longer of the two is used
pub fn parse_models(r: &Value) -> Vec<ModelOption> {
    let listed = listed_models(r);
    let options: Vec<ModelOption> = r["configOptions"].as_array().into_iter().flatten()
        .filter(|o| o["id"] == "model")
        .flat_map(|o| o["options"].as_array().into_iter().flatten())
        // Grouped options hold their values one level down
        .flat_map(|v| if v["options"].is_array() { v["options"].as_array().unwrap().clone() } else { vec![v.clone()] })
        .filter_map(|v| {
            let id = v["value"].as_str()?.to_string();
            Some(ModelOption {
                name: v["name"].as_str().unwrap_or(&id).to_string(),
                description: v["description"].as_str().unwrap_or("").to_string(),
                resolved: String::new(),
                efforts: vec![],
                default_effort: String::new(),
                id,
            })
        })
        .collect();
    if options.len() > listed.len() { options } else { listed }
}

/// Antigravity: the models are the values of its `model` option; `availableModels` lists every model+level pair
/// (`claude-opus-5-5-high`), which gives each model's thinking levels. A model without levels is listed as itself
pub fn antigravity_models(r: &Value) -> Vec<ModelOption> {
    const LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
    let variants = listed_models(r);
    let mut models: Vec<ModelOption> = r["configOptions"].as_array().into_iter().flatten()
        .filter(|o| o["id"] == "model")
        .flat_map(|o| o["options"].as_array().into_iter().flatten())
        .filter_map(|v| {
            let id = v["value"].as_str()?.to_string();
            let mut efforts: Vec<String> = LEVELS.iter()
                .filter(|l| variants.iter().any(|m| m.id.strip_prefix(id.as_str()).and_then(|t| t.strip_prefix('-')) == Some(**l)))
                .map(|l| l.to_string())
                .collect();
            if efforts.len() < 2 { efforts.clear(); }
            Some(ModelOption {
                name: v["name"].as_str().unwrap_or(&id).to_string(),
                description: String::new(),
                resolved: String::new(),
                default_effort: if efforts.iter().any(|e| e == "medium") { "medium".into() } else { String::new() },
                efforts,
                id,
            })
        })
        .collect();
    if models.is_empty() { models = variants; }
    models
}

/// Antigravity reports a used-up quota as the turn's only reply, "⚠ Individual quota reached. … Resets in 4h9m8s.",
/// and ends the turn normally. Returns the time until the reset in ms
fn antigravity_limit(text: &str) -> Option<i64> {
    if !text.to_lowercase().contains("quota reached") {
        return None;
    }
    let rest = &text[text.find("Resets in ")? + "Resets in ".len()..];
    let (mut total, mut n, mut any) = (0i64, 0i64, false);
    for c in rest.chars() {
        match c {
            '0'..='9' => n = n * 10 + c.to_digit(10).unwrap() as i64,
            'd' | 'h' | 'm' | 's' => {
                total += n * match c { 'd' => 86_400_000, 'h' => 3_600_000, 'm' => 60_000, _ => 1000 };
                n = 0;
                any = true;
            }
            _ => break,
        }
    }
    any.then_some(total)
}

fn listed_models(r: &Value) -> Vec<ModelOption> {
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
            ToolKind::Antigravity => "agy acp --login",
            _ => "登录命令",
        };
        format!("{msg}\n请先在这台机器上执行 `{cmd}` 完成登录")
    } else {
        msg.to_string()
    }
}

fn update(st: &mut State, em: &Emitter, u: &Value) {
    match u["sessionUpdate"].as_str().unwrap_or("") {
        "config_option_update" if st.parameterized => apply_config_options(st, u),
        "config_option_update" => read_thought_level(st, u),
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
        "current_mode_update" if st.native_modes => {
            // Antigravity switched modes itself (Shift+Tab equivalents, leaving plan)
            if let Some(m) = u["currentModeId"].as_str() {
                st.native_mode = Some(m.to_string());
                let perm = match m {
                    "accept-edits" => "acceptEdits",
                    "auto" => "auto",
                    "plan" => "plan",
                    "default" if st.perm == "bypass" => "bypass",
                    "default" => "default",
                    other => other,
                };
                if perm != st.perm {
                    st.perm = perm.to_string();
                    em.permission_mode(&st.perm);
                }
            }
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
        "session_info_update" => {
            if let Some(t) = u["title"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
                em.title_if_empty(t);
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
                // Antigravity puts its tool name (run_command, write_file…) at the top level
                t.name = meta_tool_name(u)
                    .or(u["name"].as_str())
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
            // An Antigravity command result was recognized (its output may be empty)
            let mut command_text = false;
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
                    "content" => {
                        let text = content_text(&c["content"]);
                        match antigravity_command_text(&text) {
                            Some(out) => {
                                command_text = true;
                                texts.push(out);
                            }
                            None => texts.push(text),
                        }
                    }
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
            if st.is_antigravity && !st.loading {
                let agy_tool = if t.name == "other" || t.name.is_empty() { t.title.as_str() } else { t.name.as_str() };
                if is_agy_subagent_tool(agy_tool, &t.input) {
                    let target = agy_recipient(&t.input).map(str::to_string);
                    if agy_tool.eq_ignore_ascii_case("send_message") {
                        if let Some(recip) = target.as_deref() {
                            st.agy_send_baselines
                                .entry(recip.to_string())
                                .or_insert_with(|| count_agy_transcript_steps(&agy_brain_dir(), recip));
                        }
                    }
                    st.agy_pending_calls.insert(id.clone(), AgyPendingCall { name: agy_tool.to_string(), target_subagent: target, desc: agy_task_desc(&t.input) });
                }
            }
            let status = u["status"].as_str().unwrap_or("");
            if (status == "completed" || status == "failed") && !t.done {
                t.done = true;
                st.subagents.remove(&id);
                let mut output = texts.join("\n");
                if output.is_empty() && !command_text && u["rawOutput"]["output"].is_string() {
                    let raw = u["rawOutput"]["output"].as_str().unwrap_or("");
                    command_text = true;
                    output = antigravity_command_text(raw).unwrap_or_else(|| raw.to_string());
                }
                if output.is_empty() && !command_text && !u["rawOutput"].is_null() {
                    output = raw_output_text(&crate::computer_use::display_value(&u["rawOutput"]));
                }
                // `agy acp` reports invoke_subagent / send_message completed immediately after spawning, with empty output.
                // Hold the ToolResult until the background subagent actually finishes and produces its result.
                let defer_agy = st.is_antigravity
                    && !st.loading
                    && status == "completed"
                    && output.is_empty()
                    && st.agy_pending_calls.contains_key(&id);
                if !defer_agy {
                    st.agy_pending_calls.remove(&id);
                    em.event(EventBody::ToolResult {
                        id: id.clone(),
                        output: crate::util::truncate(&output, 64 * 1024),
                        is_error: status == "failed",
                    });
                }
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

/// Antigravity's run_command result: "Created At: …\nCompleted At: …\nCommand exited with code N.\n\nCommand output:\n…".
/// Keep only the output, plus an "exit code: N" line when N isn't 0 (the frontend marks the call failed from it)
fn antigravity_command_text(t: &str) -> Option<String> {
    if !t.starts_with("Created At:") {
        return None;
    }
    antigravity_command_body(t)
}

/// The "Command exited with code N.\n\nCommand output:\n…" part, also used for background command results
fn antigravity_command_body(t: &str) -> Option<String> {
    let code = t.split_once("Command exited with code ")?.1.split(['.', '\n']).next()?.trim();
    let mut out = t.split_once("Command output:\n").map(|(_, o)| o.replace("\r\n", "\n").trim_end().to_string()).unwrap_or_default();
    if code != "0" {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("exit code: {code}"));
    }
    Some(out)
}

fn agy_brain_dir() -> std::path::PathBuf {
    crate::paths::home().join(".gemini").join("antigravity-cli").join("brain")
}

/// `brain/<id>/.system_generated/logs/transcript_full.jsonl` (or the compact one); None for ids that aren't a plain name
fn agy_transcript_path(brain_dir: &std::path::Path, conv_id: &str) -> Option<std::path::PathBuf> {
    if conv_id.is_empty() || conv_id.contains(['/', '\\']) || conv_id.contains("..") {
        return None;
    }
    let logs_dir = brain_dir.join(conv_id).join(".system_generated").join("logs");
    let full = logs_dir.join("transcript_full.jsonl");
    if full.is_file() {
        return Some(full);
    }
    let compact = logs_dir.join("transcript.jsonl");
    compact.is_file().then_some(compact)
}

fn parse_agy_steps(raw: &str) -> Vec<Value> {
    raw.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str::<Value>(l).ok()).collect()
}

fn read_agy_transcript_steps(brain_dir: &std::path::Path, conv_id: &str) -> Vec<Value> {
    agy_transcript_path(brain_dir, conv_id)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|raw| parse_agy_steps(&raw))
        .unwrap_or_default()
}

/// Number of steps (one per line) without parsing them: transcripts of long sessions run to megabytes
fn count_agy_transcript_steps(brain_dir: &std::path::Path, conv_id: &str) -> usize {
    agy_transcript_path(brain_dir, conv_id)
        .and_then(|p| std::fs::read(p).ok())
        .map(|raw| raw.split(|b| *b == b'\n').filter(|l| l.iter().any(|b| !b.is_ascii_whitespace())).count())
        .unwrap_or(0)
}

/// One transcript, parsed again only when its size or mtime changes (the watch polls every 250 ms)
#[derive(Debug, Default)]
struct AgyTranscript {
    stamp: Option<(u64, std::time::SystemTime)>,
    steps: std::sync::Arc<Vec<Value>>,
    /// When the file last changed, for spotting a subagent that stopped moving
    changed_at: Option<tokio::time::Instant>,
}

impl AgyTranscript {
    fn refresh(&mut self, brain_dir: &std::path::Path, conv_id: &str) -> std::sync::Arc<Vec<Value>> {
        let now = tokio::time::Instant::now();
        let Some(path) = agy_transcript_path(brain_dir, conv_id) else {
            self.changed_at.get_or_insert(now);
            return self.steps.clone();
        };
        let stamp = std::fs::metadata(&path)
            .ok()
            .map(|m| (m.len(), m.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH)));
        if stamp.is_some() && stamp != self.stamp {
            if let Ok(raw) = std::fs::read_to_string(&path) {
                self.steps = std::sync::Arc::new(parse_agy_steps(&raw));
                self.stamp = stamp;
                self.changed_at = Some(now);
            }
        }
        self.changed_at.get_or_insert(now);
        self.steps.clone()
    }
}

fn agy_recipient(v: &Value) -> Option<&str> {
    v["Recipient"]
        .as_str()
        .or_else(|| v["recipient"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn is_agy_subagent_tool(name: &str, input: &Value) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "invoke_subagent"
        || (lower == "send_message" && agy_recipient(input).is_some())
        || (lower == "swarm" && input["action"].as_str() == Some("add"))
}

fn is_agy_system(step: &Value) -> bool {
    step["type"].as_str() == Some("SYSTEM_MESSAGE") || step["source"].as_str() == Some("SYSTEM")
}

/// A SYSTEM_MESSAGE that wakes the parent: a report from a subagent or a background task. agy's own notices
/// (`sender=system`, e.g. after a settings change) arrive inside a prompt turn and wake nobody
fn is_agy_wakeup(step: &Value) -> bool {
    is_agy_system(step) && {
        let content = step["content"].as_str().unwrap_or("");
        content.contains("<SYSTEM_MESSAGE>") && !content.contains("sender=system ")
    }
}

fn is_agy_final_reply(step: &Value) -> bool {
    step["type"].as_str() == Some("PLANNER_RESPONSE")
        && step["status"].as_str() == Some("DONE")
        && step["tool_calls"].as_array().is_none_or(|a| a.is_empty())
}

/// A step that won't change any more. A background command stays a RUNNING GENERIC step for good (its output comes
/// later as a SYSTEM_MESSAGE), so it counts as settled too
fn agy_step_settled(step: &Value) -> bool {
    match step["status"].as_str() {
        Some("DONE" | "ERROR" | "INVALID") => true,
        Some("RUNNING") => step["type"].as_str() == Some("GENERIC") && agy_background_task(step["content"].as_str().unwrap_or("")).is_some(),
        _ => false,
    }
}

/// "Tool is running as a background task with task id: <conv>/task-2"
fn agy_background_task(content: &str) -> Option<&str> {
    let rest = content.split_once("Tool is running as a background task with task id: ")?.1;
    rest.split_whitespace().next().filter(|s| !s.is_empty())
}

/// A SYSTEM_MESSAGE `Task id "<id>" finished with result:\n<result>\n\nLog: …`: the task id and its result text
fn agy_task_finished(content: &str) -> Option<(&str, String)> {
    let rest = content.split_once("Task id \"")?.1;
    let (task, rest) = rest.split_once('"')?;
    let result = rest.split_once("finished with result:")?.1;
    let result = result.split("\n\nLog: ").next().unwrap_or(result);
    let result = result.split("</SYSTEM_MESSAGE>").next().unwrap_or(result).trim();
    let text = antigravity_command_body(result).unwrap_or_else(|| result.replace("\r\n", "\n"));
    Some((task, text))
}

/// Extract spawned subagent conversation IDs from `"Created the following subagent:\n{...}"` GENERIC step content
fn extract_created_subagent_ids(content: &str) -> Vec<String> {
    if !content.contains("Created the following subagent") {
        return vec![];
    }
    let mut ids = vec![];
    let mut rest = content;
    while let Some(pos) = rest.find("\"conversationId\"") {
        rest = &rest[pos + "\"conversationId\"".len()..];
        let Some(colon) = rest.find(':') else { break };
        rest = rest[colon + 1..].trim_start();
        let Some(after_quote) = rest.strip_prefix('"') else { continue };
        let Some(end_quote) = after_quote.find('"') else { break };
        let id = after_quote[..end_quote].trim();
        if !id.is_empty() && !ids.iter().any(|x| x == id) {
            ids.push(id.to_string());
        }
        rest = &after_quote[end_quote + 1..];
    }
    ids
}

/// Clean Antigravity transcript GENERIC tool outputs and append any generated media file paths
fn clean_agy_generic_output(raw: &str, media: Option<&Vec<Value>>) -> String {
    let mut out = if let Some(cmd) = antigravity_command_text(raw) {
        cmd
    } else {
        let normalized = raw.replace("\r\n", "\n");
        let lines: Vec<&str> = normalized.lines().collect();
        let mut start = 0;
        while start < lines.len() && (lines[start].starts_with("Created At:") || lines[start].starts_with("Completed At:")) {
            start += 1;
        }
        lines[start..].join("\n").trim().to_string()
    };
    for m in media.into_iter().flatten() {
        if let Some(uri) = m["uri"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
            let clean_path = uri.strip_prefix("file://").unwrap_or(uri);
            if !out.contains(clean_path) && !out.contains(uri) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(clean_path);
            }
        }
    }
    out
}

fn has_pending_agy_subagent_turn(brain_dir: &std::path::Path, parent_sid: &str, pre_turn_steps: usize) -> bool {
    let steps = read_agy_transcript_steps(brain_dir, parent_sid);
    if pre_turn_steps >= steps.len() {
        return false;
    }
    let slice = &steps[pre_turn_steps..];
    let launched = slice.iter().any(|s| {
        s["tool_calls"].as_array().is_some_and(|tcs| {
            tcs.iter().any(|tc| is_agy_subagent_tool(tc["name"].as_str().unwrap_or(""), &tc["args"]))
        }) || !extract_created_subagent_ids(s["content"].as_str().unwrap_or("")).is_empty()
    });
    if !launched {
        return false;
    }
    // Still pending unless a SYSTEM_MESSAGE already woke the parent and produced a finished empty-tool PLANNER_RESPONSE
    match slice.iter().rposition(is_agy_wakeup) {
        None => true,
        Some(wi) => !slice[wi + 1..].iter().any(is_agy_final_reply),
    }
}

/// Tool calls read from a transcript, waiting for the GENERIC steps that carry their results
#[derive(Debug, Default)]
struct AgyToolLane {
    /// `(tool_call_id, tool_name)` in call order; GENERIC results come back in the same order
    open: Vec<(String, String)>,
    /// Upcoming GENERIC steps that belong to calls we don't show (`send_message` back to the parent…)
    skip: usize,
    /// Background commands: agy task id → our tool call id; the result arrives as a SYSTEM_MESSAGE
    background: Vec<(String, String)>,
}

impl AgyToolLane {
    fn busy(&self) -> bool {
        !self.open.is_empty() || !self.background.is_empty()
    }

    /// A settled GENERIC step: the result of the oldest open call (or the start of its background task)
    fn generic(&mut self, watch_out: &mut AgyOut, step: &Value) {
        if self.skip > 0 {
            self.skip -= 1;
            return;
        }
        if self.open.is_empty() {
            return;
        }
        let (tc_id, _) = self.open.remove(0);
        let content = step["content"].as_str().unwrap_or("");
        if step["status"].as_str() == Some("RUNNING") {
            if let Some(task) = agy_background_task(content) {
                self.background.push((task.to_string(), tc_id));
                return;
            }
        }
        let out = clean_agy_generic_output(content, step["media"].as_array());
        watch_out.push(EventBody::ToolResult {
            id: tc_id,
            output: crate::util::truncate(&out, 64 * 1024),
            is_error: matches!(step["status"].as_str(), Some("ERROR" | "INVALID")),
        });
    }

    /// A SYSTEM_MESSAGE: fills a background command's result when it reports its task finished
    fn system(&mut self, watch_out: &mut AgyOut, step: &Value) {
        let Some((task, text)) = agy_task_finished(step["content"].as_str().unwrap_or("")) else {
            return;
        };
        if let Some(i) = self.background.iter().position(|(t, _)| t == task) {
            let (_, tc_id) = self.background.remove(i);
            watch_out.push(EventBody::ToolResult { id: tc_id, output: crate::util::truncate(&text, 64 * 1024), is_error: text.contains("exit code: ") });
        }
    }

    fn close_all(&mut self, watch_out: &mut AgyOut) {
        for (tc_id, _) in self.open.drain(..).chain(self.background.drain(..).map(|(_, id)| (id, String::new()))) {
            watch_out.push(EventBody::ToolResult { id: tc_id, output: String::new(), is_error: false });
        }
    }
}

/// Events produced by one pass of the watch; `AgyWatch::flush_out` sends them, reopening a detached turn first
type AgyOut = Vec<EventBody>;

#[derive(Debug, Default)]
struct AgySubagentTracker {
    start_step: usize,
    next_step: usize,
    lane: AgyToolLane,
    /// Message sent back to the parent via `send_message`
    reply: Option<String>,
    /// Last plain-text PLANNER_RESPONSE fallback if no `send_message` was used
    last_text: Option<String>,
    done: bool,
    transcript: AgyTranscript,
    /// Background task description (from the call that started it)
    desc: String,
    /// When the watch first saw it (ms), for the background task list
    started_at: i64,
    /// Stopped from the background task list
    stopped: bool,
    /// Its "finished" line was posted
    announced: bool,
    /// Its "waiting for an approval" error was posted
    stall_reported: bool,
}

#[derive(Debug, Clone)]
struct AgyPendingCall {
    name: String,
    target_subagent: Option<String>,
    /// Shown in the background task list for the subagent it starts
    desc: String,
}

/// Background task description for a subagent call: its role, else the start of its prompt / message
fn agy_task_desc(input: &Value) -> String {
    let first = input["Subagents"].as_array().and_then(|a| a.first()).unwrap_or(input);
    let pick = |v: &Value| {
        ["Role", "TypeName", "name", "Prompt", "Message", "brief", "prompt", "message"]
            .iter()
            .find_map(|k| v[*k].as_str().map(str::trim).filter(|s| !s.is_empty()))
            .map(|s| crate::util::truncate(&s.replace(char::is_whitespace, " "), 80))
    };
    pick(first).or_else(|| pick(input)).unwrap_or_default()
}

/// Running subagents handed from one watch to the next across a prompt
#[derive(Debug, Default)]
struct AgyCarry {
    subs: Vec<(String, AgySubagentTracker)>,
    calls: HashMap<String, AgyPendingCall>,
}

/// What a pass of the watch decided
#[derive(Debug)]
enum AgyStep {
    /// Keep waiting
    Wait,
    /// Finished; the request (if any) goes to agy
    Done(Option<Value>),
}

/// Antigravity subagents (`invoke_subagent`, `send_message` to one, `swarm`) run in the background: agy ends the
/// prompt turn right after launching them, and when they report back it wakes the parent by itself, outside any
/// ACP prompt, so none of that is streamed. This follows their transcripts on disk instead. Like Claude's
/// background tasks, the turn ends with the prompt; running subagents are listed as the session's background
/// tasks, and the parent's wakeup reopens the session (Running, then TurnEnd)
#[derive(Debug)]
struct AgyWatch {
    parent_sid: String,
    brain_dir: std::path::PathBuf,
    pre_turn_steps: usize,
    /// Steps in the parent's transcript when the prompt returned: everything before was streamed, so only a
    /// SYSTEM_MESSAGE after them can be the wakeup
    wake_from: usize,
    next_parent_discover_step: usize,
    stop_reason: Option<String>,
    pending_calls: HashMap<String, AgyPendingCall>,
    subagents: HashMap<String, AgySubagentTracker>,
    subagent_order: Vec<String>,
    wakeup_msg_idx: Option<usize>,
    next_parent_emit_step: usize,
    parent_lane: AgyToolLane,
    parent: AgyTranscript,
    wakeup_stable_ticks: usize,
    started_at: tokio::time::Instant,
    all_subagents_done_at: Option<tokio::time::Instant>,
    /// Allow-all launches agy with --dangerously-skip-permissions, so subagents never wait for approval there
    stall_check: bool,
    /// The turn already ended (TurnEnd sent); the parent's wakeup reopens it with a Running status first
    detached: bool,
    reopened: bool,
    /// This pass produced the parent's own events (its wakeup), which reopen a detached turn
    wake: bool,
    out: AgyOut,
}

impl AgyWatch {
    fn new(
        parent_sid: String,
        brain_dir: std::path::PathBuf,
        pre_turn_steps: usize,
        wake_from: usize,
        stop_reason: Option<String>,
        pending_calls: HashMap<String, AgyPendingCall>,
        send_baselines: HashMap<String, usize>,
        stall_check: bool,
    ) -> Self {
        let mut watch = Self {
            parent_sid,
            brain_dir,
            pre_turn_steps,
            wake_from: wake_from.max(pre_turn_steps),
            next_parent_discover_step: pre_turn_steps,
            stop_reason,
            pending_calls,
            subagents: HashMap::new(),
            subagent_order: vec![],
            wakeup_msg_idx: None,
            next_parent_emit_step: pre_turn_steps,
            parent_lane: AgyToolLane::default(),
            parent: AgyTranscript::default(),
            wakeup_stable_ticks: 0,
            started_at: tokio::time::Instant::now(),
            all_subagents_done_at: None,
            stall_check,
            detached: false,
            reopened: false,
            wake: false,
            out: vec![],
        };
        for (sid, baseline) in send_baselines {
            watch.add_subagent(sid, Some(baseline));
        }
        watch
    }

    fn add_subagent(&mut self, sub_id: String, explicit_start: Option<usize>) {
        if self.subagents.contains_key(&sub_id) {
            return;
        }
        let start_step = explicit_start.unwrap_or_else(|| {
            // Reused via send_message: start at the last message from this parent, else from the beginning
            let steps = read_agy_transcript_steps(&self.brain_dir, &sub_id);
            steps
                .iter()
                .rposition(|s| is_agy_system(s) && s["content"].as_str().unwrap_or("").contains(&self.parent_sid))
                .unwrap_or(0)
        });
        self.subagent_order.push(sub_id.clone());
        self.subagents.insert(
            sub_id,
            AgySubagentTracker { start_step, next_step: start_step, started_at: crate::util::now_ms(), ..Default::default() },
        );
        self.all_subagents_done_at = None;
    }

    /// Take over subagents still running from the previous watch
    fn adopt(&mut self, carry: AgyCarry) {
        for (id, t) in carry.subs {
            if !self.subagents.contains_key(&id) {
                self.subagent_order.push(id.clone());
                self.subagents.insert(id, t);
                self.all_subagents_done_at = None;
            }
        }
        self.pending_calls.extend(carry.calls);
    }

    /// Running subagents as background tasks
    fn tasks(&mut self) -> Vec<BackgroundTask> {
        let mut v = vec![];
        for id in &self.subagent_order {
            let Some(t) = self.subagents.get_mut(id) else { continue };
            if t.desc.is_empty() {
                if let Some(c) = self.pending_calls.values().find(|c| c.target_subagent.as_deref() == Some(id.as_str())) {
                    t.desc = c.desc.clone();
                }
            }
            if !t.done {
                v.push(BackgroundTask {
                    id: id.clone(),
                    kind: "local_agent".into(),
                    description: if t.desc.is_empty() { id.chars().take(8).collect() } else { t.desc.clone() },
                    started_at: t.started_at,
                    output_file: None,
                });
            }
        }
        v
    }

    /// Mark a subagent stopped from the background task list; the caller sends session/cancel for it
    fn stop(&mut self, id: &str) -> bool {
        let _ = self.tasks();
        let Some(t) = self.subagents.get_mut(id).filter(|t| !t.done) else {
            return false;
        };
        t.done = true;
        t.stopped = true;
        t.announced = true;
        let name = if t.desc.is_empty() { id.chars().take(8).collect() } else { t.desc.clone() };
        t.lane.close_all(&mut self.out);
        // Its call won't get a report; close it now (a restart waiting for the task list to empty comes next)
        let calls: Vec<String> =
            self.pending_calls.iter().filter(|(_, c)| c.target_subagent.as_deref() == Some(id)).map(|(k, _)| k.clone()).collect();
        for k in calls {
            self.pending_calls.remove(&k);
            self.out.push(EventBody::ToolResult { id: k, output: String::new(), is_error: true });
        }
        self.out.push(EventBody::System { text: format!("后台任务已停止：子 agent {name}") });
        true
    }

    fn running_subagents(&self) -> Vec<String> {
        self.subagent_order.iter().filter(|id| self.subagents.get(*id).is_some_and(|t| !t.done)).cloned().collect()
    }

    /// Send what this pass produced. A detached turn is reopened (status Running) before the parent's wakeup;
    /// the subagents' own events go out while the session is idle, like a background task's
    fn flush_out(&mut self, em: &Emitter) {
        if self.out.is_empty() {
            return;
        }
        if self.detached && !self.reopened && self.wake {
            self.reopened = true;
            em.status(SessionStatus::Running, None);
        }
        for body in self.out.drain(..) {
            em.event(body);
        }
    }
}

fn subagent_summary_text(t: &AgySubagentTracker) -> String {
    if t.stopped {
        return String::new();
    }
    t.reply.clone().or_else(|| t.last_text.clone()).unwrap_or_default()
}

fn extract_system_message_content(msg: &str) -> Option<String> {
    let (_, rest) = msg.split_once("content=")?;
    let trimmed = rest.split("</SYSTEM_MESSAGE>").next().unwrap_or(rest).trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn flush_ready_pending_calls(watch: &mut AgyWatch, force_all: bool) {
    let all_done = !watch.subagents.is_empty() && watch.subagents.values().all(|t| t.done);
    let combined: String = watch
        .subagent_order
        .iter()
        .filter_map(|sid| watch.subagents.get(sid))
        .map(subagent_summary_text)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut done = vec![];
    for (call_id, call) in &watch.pending_calls {
        if let Some(target) = call.target_subagent.as_deref() {
            if force_all || watch.subagents.get(target).is_some_and(|t| t.done) {
                let output = watch.subagents.get(target).map(subagent_summary_text).filter(|s| !s.is_empty()).unwrap_or_else(|| combined.clone());
                done.push((call_id.clone(), output));
            }
        } else if all_done || force_all {
            done.push((call_id.clone(), combined.clone()));
        }
    }
    for (call_id, output) in done {
        watch.pending_calls.remove(&call_id);
        watch.out.push(EventBody::ToolResult { id: call_id, output: crate::util::truncate(&output, 64 * 1024), is_error: false });
    }
}

fn finish_agy_watch(watch: &mut AgyWatch, em: &Emitter) {
    flush_ready_pending_calls(watch, true);
    for sid in watch.subagent_order.clone() {
        if let Some(t) = watch.subagents.get_mut(&sid) {
            t.lane.close_all(&mut watch.out);
        }
    }
    watch.parent_lane.close_all(&mut watch.out);
    watch.flush_out(em);
}

/// Stop following the subagents (the adapter exits)
fn drop_agy_watch(st: &mut State, em: &Emitter) {
    if let Some(mut watch) = st.agy_watch.take() {
        finish_agy_watch(&mut watch, em);
        em.background_tasks(vec![]);
    }
}

/// End the watch but keep its running subagents (with their calls still waiting for a report) in `st.agy_carry`:
/// a new prompt is going out (the parent's own steps would confuse the watch), or the wakeup turn was interrupted
fn carry_agy_watch(st: &mut State, em: &Emitter) {
    let Some(mut watch) = st.agy_watch.take() else {
        return;
    };
    for id in watch.running_subagents() {
        if let Some(t) = watch.subagents.remove(&id) {
            st.agy_carry.subs.push((id, t));
        }
    }
    watch.subagent_order.retain(|id| watch.subagents.contains_key(id));
    let carried: Vec<String> = st.agy_carry.subs.iter().map(|(id, _)| id.clone()).collect();
    let calls: Vec<String> = watch
        .pending_calls
        .iter()
        .filter(|(_, c)| c.target_subagent.as_ref().is_some_and(|t| carried.contains(t)))
        .map(|(k, _)| k.clone())
        .collect();
    for k in calls {
        if let Some(c) = watch.pending_calls.remove(&k) {
            st.agy_carry.calls.insert(k, c);
        }
    }
    finish_agy_watch(&mut watch, em);
}

/// session/cancel for a subagent stopped from the background task list (session/close doesn't stop it)
fn stop_agy_subagent(st: &mut State, em: &Emitter, id: &str) -> Option<Value> {
    let in_watch = st.agy_watch.as_mut().is_some_and(|w| w.stop(id));
    let stopped = match st.agy_watch.as_mut() {
        Some(w) if in_watch => {
            w.flush_out(em);
            em.background_tasks(w.tasks());
            true
        }
        _ => {
            let pos = st.agy_carry.subs.iter().position(|(sid, t)| sid == id && !t.done)?;
            let (_, mut t) = st.agy_carry.subs.remove(pos);
            let mut out = vec![];
            t.lane.close_all(&mut out);
            for e in out {
                em.event(e);
            }
            st.agy_carry.calls.retain(|k, c| {
                let mine = c.target_subagent.as_deref() == Some(id);
                if mine {
                    em.event(EventBody::ToolResult { id: k.clone(), output: String::new(), is_error: true });
                }
                !mine
            });
            let name = if t.desc.is_empty() { id.chars().take(8).collect() } else { t.desc };
            em.event(EventBody::System { text: format!("后台任务已停止：子 agent {name}") });
            let left: Vec<BackgroundTask> = em_carry_tasks(&st.agy_carry);
            em.background_tasks(left);
            true
        }
    };
    stopped.then(|| json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}))
}

fn em_carry_tasks(c: &AgyCarry) -> Vec<BackgroundTask> {
    c.subs
        .iter()
        .map(|(id, t)| BackgroundTask {
            id: id.clone(),
            kind: "local_agent".into(),
            description: if t.desc.is_empty() { id.chars().take(8).collect() } else { t.desc.clone() },
            started_at: t.started_at,
            output_file: None,
        })
        .collect()
}

fn poll_agy_watch(st: &mut State, spec: &StartSpec, em: &Emitter) -> Option<Value> {
    let mut watch = st.agy_watch.take()?;
    match step_agy_watch(&mut watch, spec, em, &mut st.next_id, &mut st.pending) {
        AgyStep::Wait => {
            st.agy_watch = Some(watch);
            None
        }
        AgyStep::Done(req) => {
            if !watch.detached {
                st.prompting = false;
                st.end_subagents();
                em.event(EventBody::TurnEnd { stop_reason: watch.stop_reason, model: None });
            } else if watch.reopened {
                em.event(EventBody::TurnEnd { stop_reason: Some("end_turn".into()), model: None });
            }
            req
        }
    }
}

/// Advance the Antigravity subagent + parent-wakeup watch by one tick
fn step_agy_watch(
    watch: &mut AgyWatch,
    spec: &StartSpec,
    em: &Emitter,
    next_id: &mut u64,
    pending: &mut HashMap<u64, Pending>,
) -> AgyStep {
    let step = step_agy_watch_inner(watch, spec, next_id, pending);
    if matches!(step, AgyStep::Done(_)) {
        // Close whatever is still open (Task calls without a report, subagent tool calls without a result)
        finish_agy_watch(watch, em);
    } else {
        watch.flush_out(em);
    }
    em.background_tasks(if matches!(step, AgyStep::Done(_)) { vec![] } else { watch.tasks() });
    step
}

fn step_agy_watch_inner(watch: &mut AgyWatch, spec: &StartSpec, next_id: &mut u64, pending: &mut HashMap<u64, Pending>) -> AgyStep {
    use std::time::Duration;
    /// Give up on subagents whose transcripts (and the parent's) haven't moved for this long
    const IDLE_TIMEOUT: Duration = Duration::from_secs(1800);
    const DISCOVER_GRACE: Duration = Duration::from_secs(3);
    const WAKEUP_GRACE: Duration = Duration::from_secs(20);
    /// A subagent whose transcript ends in a tool call without a result, and that doesn't move for this long, is
    /// waiting for an approval: agy never sends subagent approvals over ACP
    const STALL: Duration = Duration::from_secs(30);

    let parent_steps = watch.parent.refresh(&watch.brain_dir, &watch.parent_sid);
    let idle_for = || {
        watch
            .subagents
            .values()
            .filter_map(|t| t.transcript.changed_at)
            .chain(watch.parent.changed_at)
            .chain(std::iter::once(watch.started_at))
            .max()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    };
    let timed_out = idle_for() >= IDLE_TIMEOUT;

    // 1. Discover newly spawned/messaged subagents from parent transcript steps since pre_turn_steps
    while watch.next_parent_discover_step < parent_steps.len() {
        let step = &parent_steps[watch.next_parent_discover_step];
        if !agy_step_settled(step) {
            break;
        }
        match step["type"].as_str().unwrap_or("") {
            "PLANNER_RESPONSE" => {
                for tc in step["tool_calls"].as_array().into_iter().flatten() {
                    if tc["name"].as_str() == Some("send_message") {
                        if let Some(recip) = agy_recipient(&tc["args"]) {
                            watch.add_subagent(recip.to_string(), None);
                        }
                    }
                }
            }
            "GENERIC" => {
                for sub_id in extract_created_subagent_ids(step["content"].as_str().unwrap_or("")) {
                    watch.add_subagent(sub_id.clone(), Some(0));
                    // Bind an unbound invoke_subagent call to the first subagent it created
                    if let Some(call) = watch.pending_calls.values_mut().find(|c| c.target_subagent.is_none() && c.name.eq_ignore_ascii_case("invoke_subagent")) {
                        call.target_subagent = Some(sub_id.clone());
                    }
                }
            }
            _ => {}
        }
        watch.next_parent_discover_step += 1;
    }

    if watch.subagents.is_empty() {
        if watch.started_at.elapsed() >= DISCOVER_GRACE {
            return AgyStep::Done(None);
        }
        return AgyStep::Wait;
    }

    // 2. Poll each subagent's own transcript for live tool calls, results, and final reply
    let parent_sid = watch.parent_sid.clone();
    for sub_id in watch.subagent_order.clone() {
        let Some(tracker) = watch.subagents.get_mut(&sub_id) else { continue };
        let sub_steps = tracker.transcript.refresh(&watch.brain_dir, &sub_id);
        while tracker.next_step < sub_steps.len() {
            let step = &sub_steps[tracker.next_step];
            if !agy_step_settled(step) {
                break;
            }
            let step_idx = tracker.next_step;
            match step["type"].as_str().unwrap_or("") {
                "PLANNER_RESPONSE" => {
                    if let Some(tcs) = step["tool_calls"].as_array().filter(|a| !a.is_empty()) {
                        for (tc_idx, tc) in tcs.iter().enumerate() {
                            let tc_name = tc["name"].as_str().unwrap_or("tool");
                            let tc_args = tc["args"].clone();
                            if tc_name == "send_message" && agy_recipient(&tc_args) == Some(parent_sid.as_str()) {
                                if let Some(msg) = tc_args["Message"].as_str().or_else(|| tc_args["message"].as_str()) {
                                    tracker.reply = Some(msg.to_string());
                                }
                                tracker.lane.skip += 1;
                            } else {
                                // Transcript tool calls carry no id; the prefix also tells the frontend they're the subagent's
                                let tc_id = tc["id"].as_str().map(str::to_string).unwrap_or_else(|| format!("agy-sub:{sub_id}:{step_idx}:{tc_idx}"));
                                watch.out.push(EventBody::ToolCall { id: tc_id.clone(), name: tc_name.to_string(), title: tc_name.to_string(), input: tc_args, diffs: vec![] });
                                tracker.lane.open.push((tc_id, tc_name.to_string()));
                            }
                        }
                    } else if let Some(text) = step["content"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        tracker.last_text = Some(text.to_string());
                    }
                }
                "GENERIC" => {
                    tracker.lane.generic(&mut watch.out, step);
                    if tracker.lane.skip == 0 && tracker.reply.is_some() && !tracker.lane.busy() {
                        tracker.done = true;
                    }
                }
                "SYSTEM_MESSAGE" => tracker.lane.system(&mut watch.out, step),
                _ => {}
            }
            tracker.next_step += 1;
        }
        let ended = tracker.next_step > tracker.start_step && sub_steps.get(tracker.next_step - 1).is_some_and(is_agy_final_reply);
        if !tracker.done && !tracker.lane.busy() && ended {
            tracker.done = true;
        }
        // Waiting on a tool call that never got a result, with the transcript at rest: an approval nobody can give.
        // Said once; it stays in the background task list, where it can be stopped
        if watch.stall_check
            && !tracker.done
            && !tracker.stall_reported
            && !tracker.lane.open.is_empty()
            && tracker.next_step == sub_steps.len()
            && tracker.transcript.changed_at.is_some_and(|t| t.elapsed() >= STALL)
        {
            tracker.stall_reported = true;
            watch.out.push(EventBody::Error {
                message: "子 agent 在等待审批，但 agy 不会把子 agent 的审批请求转给 Yi Console。在后台任务里停止它，切到「全部放行」后重试，或在终端模式里处理".into(),
            });
        }
    }

    // 3. Check parent transcript for SYSTEM_MESSAGE notifications from finished subagent(s)
    for (idx, step) in parent_steps.iter().enumerate().skip(watch.pre_turn_steps) {
        if !is_agy_system(step) || step["status"].as_str() != Some("DONE") {
            continue;
        }
        let content = step["content"].as_str().unwrap_or("");
        for sub_id in &watch.subagent_order {
            // `sender=<sub>` but not `sender=<sub>/task-N` (a background command of the subagent)
            if content.contains(&format!("sender={sub_id} ")) || content.contains(&format!("sender={sub_id}\n")) {
                if let Some(tracker) = watch.subagents.get_mut(sub_id) {
                    tracker.done = true;
                    if tracker.reply.is_none() {
                        tracker.reply = extract_system_message_content(content);
                    }
                }
            }
        }
        if idx >= watch.wake_from && is_agy_wakeup(step) && watch.wakeup_msg_idx.is_none_or(|prev| idx >= prev) {
            if watch.wakeup_msg_idx.is_none() {
                watch.next_parent_emit_step = idx + 1;
            }
            watch.wakeup_msg_idx = Some(idx);
        }
    }

    // A line for each subagent that finished, like Claude's background task notices (format parsed by ChatView)
    let _ = watch.tasks();
    for id in watch.subagent_order.clone() {
        if let Some(t) = watch.subagents.get_mut(&id).filter(|t| t.done && !t.announced) {
            t.announced = true;
            let name = if t.desc.is_empty() { id.chars().take(8).collect() } else { t.desc.clone() };
            watch.out.push(EventBody::System { text: format!("后台任务完成：子 agent {name}") });
        }
    }

    flush_ready_pending_calls(watch, false);

    // 4. Wait until all tracked subagents are finished
    if !watch.subagents.values().all(|t| t.done) {
        return if timed_out { AgyStep::Done(None) } else { AgyStep::Wait };
    }

    let done_at = *watch.all_subagents_done_at.get_or_insert_with(tokio::time::Instant::now);

    // 5. Stream & capture parent's reactive wakeup turn once SYSTEM_MESSAGE arrives
    if let Some(wakeup_idx) = watch.wakeup_msg_idx {
        while watch.next_parent_emit_step < parent_steps.len() {
            let step_idx = watch.next_parent_emit_step;
            let step = &parent_steps[step_idx];
            if !agy_step_settled(step) {
                watch.wakeup_stable_ticks = 0;
                break;
            }
            match step["type"].as_str().unwrap_or("") {
                "PLANNER_RESPONSE" => {
                    if let Some(thinking) = step["thinking"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        watch.out.push(EventBody::Thinking { id: format!("agy-wake-{}-t-{}", watch.parent_sid, step_idx), text: thinking.to_string() });
                    }
                    for (tc_idx, tc) in step["tool_calls"].as_array().into_iter().flatten().enumerate() {
                        let tc_name = tc["name"].as_str().unwrap_or("tool");
                        let tc_args = tc["args"].clone();
                        let tc_id = tc["id"].as_str().map(str::to_string).unwrap_or_else(|| format!("agy-wake:{}:{}:{}", watch.parent_sid, step_idx, tc_idx));
                        watch.out.push(EventBody::ToolCall { id: tc_id.clone(), name: tc_name.to_string(), title: tc_name.to_string(), input: tc_args.clone(), diffs: vec![] });
                        if is_agy_subagent_tool(tc_name, &tc_args) {
                            let target = agy_recipient(&tc_args).map(str::to_string);
                            watch.pending_calls.insert(tc_id, AgyPendingCall { name: tc_name.to_string(), target_subagent: target.clone(), desc: agy_task_desc(&tc_args) });
                            if let Some(recip) = target {
                                watch.add_subagent(recip, None);
                            }
                            // Its GENERIC step ("Message sent" / "Created the following subagent") isn't a shown result
                            watch.parent_lane.skip += 1;
                        } else {
                            watch.parent_lane.open.push((tc_id, tc_name.to_string()));
                        }
                    }
                    if let Some(text) = step["content"].as_str().filter(|s| !s.trim().is_empty()) {
                        watch.out.push(EventBody::AssistantText { id: format!("agy-wake-{}-a-{}", watch.parent_sid, step_idx), text: text.to_string() });
                    }
                }
                "GENERIC" => {
                    for sub_id in extract_created_subagent_ids(step["content"].as_str().unwrap_or("")) {
                        if let Some(call) = watch.pending_calls.values_mut().find(|c| c.target_subagent.is_none() && c.name.eq_ignore_ascii_case("invoke_subagent")) {
                            call.target_subagent = Some(sub_id.clone());
                        }
                        watch.add_subagent(sub_id, Some(0));
                    }
                    watch.parent_lane.generic(&mut watch.out, step);
                }
                "SYSTEM_MESSAGE" => watch.parent_lane.system(&mut watch.out, step),
                _ => {}
            }
            watch.next_parent_emit_step += 1;
            watch.wakeup_stable_ticks = 0;
            watch.wake = true;
        }

        // Did the wakeup turn launch another chained subagent? If so, wait for it
        if !watch.subagents.values().all(|t| t.done) {
            return AgyStep::Wait;
        }

        let has_planner_after_wakeup = parent_steps[wakeup_idx + 1..].iter().any(|s| s["type"].as_str() == Some("PLANNER_RESPONSE"));
        let last_is_final_planner = parent_steps.last().is_some_and(is_agy_final_reply);
        if has_planner_after_wakeup && last_is_final_planner && !watch.parent_lane.busy() && watch.next_parent_emit_step == parent_steps.len() {
            watch.wakeup_stable_ticks += 1;
            if watch.wakeup_stable_ticks >= 2 {
                flush_ready_pending_calls(watch, true);
                // agy only marks steps it streamed as seen; resuming moves that mark past the wakeup turn, so the
                // next prompt doesn't replay it (session/resume sends no history)
                *next_id += 1;
                pending.insert(*next_id, Pending::Other);
                return AgyStep::Done(Some(json!({
                    "jsonrpc": "2.0",
                    "id": *next_id,
                    "method": "session/resume",
                    "params": {"sessionId": watch.parent_sid, "cwd": spec.cwd.display().to_string(), "mcpServers": spec.mcp_servers}
                })));
            }
            return AgyStep::Wait;
        }
    }

    if done_at.elapsed() >= WAKEUP_GRACE || timed_out {
        let emitted_wakeup_text = watch.wakeup_msg_idx.is_some_and(|wi| {
            parent_steps[wi + 1..watch.next_parent_emit_step.min(parent_steps.len())]
                .iter()
                .any(|s| s["type"].as_str() == Some("PLANNER_RESPONSE") && !s["content"].as_str().unwrap_or("").trim().is_empty())
        });
        if !emitted_wakeup_text {
            let fallback: String = watch.subagent_order.iter().filter_map(|sid| watch.subagents.get(sid)).map(subagent_summary_text).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n");
            if !fallback.is_empty() {
                watch.wake = true;
                watch.out.push(EventBody::AssistantText { id: format!("agy-sub-fallback-{}", watch.parent_sid), text: fallback });
            }
        }
        flush_ready_pending_calls(watch, true);
        return AgyStep::Done(None);
    }

    AgyStep::Wait
}

/// ACP `usage` in a session/prompt result. Antigravity's totalTokens is inputTokens + cachedReadTokens + outputTokens
/// (inputTokens excludes cache hits; outputTokens includes thinking)
fn prompt_usage(u: &Value) -> Option<Usage> {
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    let usage = Usage {
        input_tokens: n("inputTokens"),
        output_tokens: n("outputTokens"),
        cache_read_tokens: n("cachedReadTokens"),
        cache_write_tokens: n("cachedWriteTokens"),
        ..Default::default()
    };
    (usage.input_tokens + usage.output_tokens + usage.cache_read_tokens > 0).then_some(usage)
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
        apply_config_options(&mut st, &config());
        publish_config(&st, &em);
        assert_eq!(st.config_options.len(), 2);
        assert_eq!(hub.session_models("s").unwrap()["current"], "gpt-test");
        // The user's choice is recorded on the session before it is sent
        crate::hub::test_set_tool(&hub, "s", ToolKind::Cursor);
        hub.set_model_config("s", "fast", "true").unwrap();
        st.config_queue = vec![("fast".into(), "true".into()), ("reasoning".into(), "high".into())];
        let first = next_config(&mut st).unwrap();
        assert_eq!(first["method"], "session/set_config_option");
        assert_eq!(first["params"]["configId"], "fast");
        assert!(config_pending(&st));
        assert!(next_config(&mut st).is_none(), "do not overlap parameter requests");
        st.pending.remove(&first["id"].as_u64().unwrap());
        let mut ack = config();
        ack["configOptions"][2]["currentValue"] = json!("true");
        st.config_inflight = None;
        apply_config_options(&mut st, &ack);
        publish_config(&st, &em);
        assert_eq!(next_config(&mut st).unwrap()["params"]["configId"], "reasoning");
        let saved = hub.store.load_sessions().unwrap();
        assert_eq!(saved[0].model_config_options[0].current_value, "true");
    }

    #[test]
    fn grok_reasoning_category_and_original_id_are_preserved() {
        let mut st = State { session_id: Some("ext".into()), parameterized: true, ..Default::default() };
        let mut reported = config();
        reported["configOptions"][3]["id"] = json!("reasoning_effort");
        apply_config_options(&mut st, &reported);
        let effort = st.config_options.iter().find(|o| o.id == "reasoning_effort").unwrap();
        assert_eq!(effort.category, "thought_level");
        let wire = serde_json::to_value(effort).unwrap();
        assert_eq!(wire["category"], "thought_level");
        st.config_queue.push(("reasoning_effort".into(), "high".into()));
        assert_eq!(next_config(&mut st).unwrap()["params"]["configId"], "reasoning_effort");
        let mut legacy = wire;
        legacy.as_object_mut().unwrap().remove("category");
        assert!(serde_json::from_value::<ModelConfigOption>(legacy).unwrap().category.is_empty());
    }

    fn cursor_spec(model: &str) -> StartSpec {
        StartSpec {
            tool: ToolKind::Cursor, mode: SessionMode::Structured, cwd: "/tmp".into(), resume: None, replay: false,
            replay_skip: 0, model: Some(model.into()), effort: None, model_config_options: vec![],
            permission_mode: "default".into(), command: None, program: "cursor-agent".into(), extra_args: vec![],
            mcp_servers: vec![], mcp_fingerprint: String::new(), allow_mcp: vec![], system_prompt: None, system_notice: false, cols: 80, rows: 24, tmux: None, session_id: None,
        }
    }

    #[test]
    fn cursor_start_switches_explicitly_and_retries_a_failed_switch() {
        let (hub, em, _rx) = crate::hub::test_emitter();
        crate::hub::test_set_tool(&hub, "s", ToolKind::Cursor);
        let spec = cursor_spec("grok-4.7[context=500k]");
        let mut st = State::default();
        st.pending.insert(1, Pending::Session);
        // session/new lists mode and model only, and Cursor is already on the wanted model
        let created = json!({"id":1,"result":{"sessionId":"ext","models":{"currentModelId":"grok-4.7",
            "availableModels":[{"modelId":"grok-4.7","name":"Grok 4.7"}]},
            "configOptions":[{"id":"model","category":"model","currentValue":"grok-4.7"}]}});
        handle(&mut st, &spec, &created, &em).unwrap();
        assert!(hub.store.model_config("grok-4.7").unwrap().is_none(), "session/new is not a parameter list");
        assert_eq!(st.config_queue, [("model".to_string(), "grok-4.7".to_string()), ("context".into(), "500k".into())]);
        let switch = next_config(&mut st).unwrap();
        // First failure: retried at once, no error shown
        let fail = |id: &Value| json!({"id":id,"error":{"code":-32602,"message":"Invalid params","data":{"message":"Invalid model value: grok-4.7"}}});
        handle(&mut st, &spec, &fail(&switch["id"]), &em).unwrap();
        let again = next_config(&mut st).unwrap();
        assert_eq!(again["params"]["value"], "grok-4.7");
        // Second failure: the prompt may go; the switch and its parameters wait for the next message
        handle(&mut st, &spec, &fail(&again["id"]), &em).unwrap();
        assert!(!config_pending(&st));
        assert_eq!(st.model_failed.as_deref(), Some("grok-4.7"));
        assert!(retry_failed_model(&mut st));
        assert_eq!(st.config_queue, [("model".to_string(), "grok-4.7".to_string()), ("context".into(), "500k".into())]);
        let switch = next_config(&mut st).unwrap();
        let ok = json!({"id":switch["id"],"result":{"configOptions":[{"id":"model","category":"model","currentValue":"grok-4.7"},
            {"id":"context","name":"Context","category":"model_config","type":"select","currentValue":"256k",
             "options":[{"value":"256k","name":"256K"},{"value":"500k","name":"500K"}]}]}});
        handle(&mut st, &spec, &ok, &em).unwrap();
        assert_eq!(hub.store.model_config("grok-4.7").unwrap().unwrap()[0].id, "context");
        assert_eq!(next_config(&mut st).unwrap()["params"]["value"], "500k");
    }

    #[test]
    fn cursor_models_come_from_the_model_option_when_the_list_is_short() {
        let r = json!({"models":{"availableModels":[{"modelId":"grok-4.7","name":"Grok 4.7"}]},
            "configOptions":[{"id":"model","category":"model","currentValue":"grok-4.7",
                "options":[{"value":"auto-smart","name":"Auto"},{"value":"grok-4.7","name":"Grok 4.7"},{"value":"gemini-4-argon","name":"Gemini 4 Argon"}]}]});
        assert_eq!(parse_models(&r).iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["auto-smart", "grok-4.7", "gemini-4-argon"]);
        let (hub, _em, _rx) = crate::hub::test_emitter();
        // A short answer (Cursor's own model request failed) neither replaces the saved list nor limits the session
        let many: Vec<_> = (0..32).map(|i| ModelOption { id: format!("m{i}"), name: format!("M{i}"), description: String::new(),
            resolved: String::new(), efforts: vec![], default_effort: String::new() }).collect();
        hub.remember_models(ToolKind::Cursor, &many);
        crate::hub::test_set_tool(&hub, "s", ToolKind::Cursor);
        _em.models(parse_models(&r));
        assert_eq!(hub.cached_models(ToolKind::Cursor).len(), 32);
        assert_eq!(hub.session_models("s").unwrap()["models"].as_array().unwrap().len(), 32);
    }

    #[test]
    fn switching_models_drops_the_previous_models_parameters_before_the_session_exists() {
        let mut st = State { want_model: Some("gemini-4-argon".into()), spec_params_stale: true, ..Default::default() };
        let (_hub, em, _rx) = crate::hub::test_emitter();
        let mut spec = cursor_spec("grok-4.7");
        spec.model_config_options = vec![ModelConfigOption { id: "fast".into(), name: "Fast".into(), category: "model_config".into(),
            description: String::new(), current_value: "true".into(), options: vec![] }];
        st.pending.insert(1, Pending::Session);
        let created = json!({"id":1,"result":{"sessionId":"ext","models":{"currentModelId":"grok-4.7",
            "availableModels":[{"modelId":"grok-4.7"},{"modelId":"gemini-4-argon"}]},
            "configOptions":[{"id":"model","category":"model","currentValue":"grok-4.7"}]}});
        handle(&mut st, &spec, &created, &em).unwrap();
        assert_eq!(st.config_queue, [("model".to_string(), "gemini-4-argon".to_string())]);
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

    /// Samples cut from `agy acp` 1.5.7
    #[test]
    fn antigravity_tool_calls_modes_and_usage() {
        use super::*;
        let out = "Created At: 2026-11-13T05:25:00+08:00\nCompleted At: 2026-11-13T05:25:00+08:00\nCommand exited with code 0.\n\nCommand output:\n./hello.txt:1:hello world\r\n";
        assert_eq!(antigravity_command_text(out).as_deref(), Some("./hello.txt:1:hello world"));
        let failed = "Created At: x\nCompleted At: y\nCommand exited with code 2.\n\nCommand output:\nls: cannot access 'nope'\r\n";
        assert_eq!(antigravity_command_text(failed).as_deref(), Some("ls: cannot access 'nope'\nexit code: 2"));
        assert_eq!(antigravity_command_text("plain output"), None);
        let silent = "Created At: x\nCompleted At: y\nCommand exited with code 0.\n\nNo output from the command.";
        assert_eq!(antigravity_command_text(silent).as_deref(), Some(""));

        let (hub, em, _rx) = crate::hub::test_emitter();
        let mut st = State { native_modes: true, perm: "default".into(), session_id: Some("ext".into()), ..Default::default() };
        update(&mut st, &em, &json!({"sessionUpdate":"tool_call","toolCallId":"c1","name":"run_command","kind":"execute",
            "title":"ls -la","status":"in_progress","rawInput":{"CommandLine":"ls -la"}}));
        update(&mut st, &em, &json!({"sessionUpdate":"tool_call_update","toolCallId":"c1","name":"run_command","kind":"execute",
            "status":"completed","rawOutput":{"output":out,"exit_code":0},
            "content":[{"type":"content","content":{"type":"text","text":out}}]}));
        update(&mut st, &em, &json!({"sessionUpdate":"tool_call","toolCallId":"c2","name":"run_command","kind":"execute","title":"git status"}));
        update(&mut st, &em, &json!({"sessionUpdate":"tool_call_update","toolCallId":"c2","status":"completed",
            "rawOutput":{"output":silent,"exit_code":0},"content":[{"type":"content","content":{"type":"text","text":silent}}]}));
        // The live echo of the prompt is dropped (the hub records the user message itself)
        update(&mut st, &em, &json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hi"}}));
        update(&mut st, &em, &json!({"sessionUpdate":"session_info_update","title":"Create And Check File"}));
        let evs = hub.store.events_since("s", 0, 100).unwrap();
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::ToolCall { name, .. } if name == "run_command")));
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::ToolResult { output, is_error: false, .. } if output == "./hello.txt:1:hello world")));
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::ToolResult { id, output, .. } if id == "c2" && output.is_empty())));
        flush(&mut st, &em);
        assert!(!hub.store.events_since("s", 0, 100).unwrap().iter().any(|e| matches!(e.body, EventBody::UserMessage { .. })));

        // Every permission mode is the tool's own ACP mode; allow-all stays in default
        st.native_mode = Some("default".into());
        st.perm = "auto".into();
        assert_eq!(apply_mode(&mut st, false).unwrap()["params"]["modeId"], "auto");
        assert!(apply_mode(&mut st, false).is_none());
        st.perm = "bypass".into();
        assert_eq!(apply_mode(&mut st, false).unwrap()["params"]["modeId"], "default");
        update(&mut st, &em, &json!({"sessionUpdate":"current_mode_update","currentModeId":"default"}));
        assert_eq!(st.perm, "bypass");
        update(&mut st, &em, &json!({"sessionUpdate":"current_mode_update","currentModeId":"accept-edits"}));
        assert_eq!(st.perm, "acceptEdits");

        let u = prompt_usage(&json!({"inputTokens":2132,"outputTokens":5,"cachedReadTokens":6081,"cachedWriteTokens":0,"thoughtTokens":0,"totalTokens":8218})).unwrap();
        assert_eq!((u.input_tokens, u.output_tokens, u.cache_read_tokens), (2132, 5, 6081));
        assert!(prompt_usage(&json!({})).is_none());
    }

    #[test]
    fn antigravity_models_levels_and_limit() {
        use super::*;
        let r = json!({
            "models":{"currentModelId":"gemini-4-argon-medium","availableModels":[
                {"modelId":"gemini-4-argon-medium"},{"modelId":"gemini-4-argon-high"},{"modelId":"gemini-4-argon-low"},
                {"modelId":"gemini-4-hydrogen-high"},{"modelId":"gemini-4-hydrogen-xhigh"},{"modelId":"gemini-4-hydrogen-low"},
                {"modelId":"gemini-4-hydrogen-medium"},{"modelId":"gpt-oss-120b-medium"}]},
            "configOptions":[
                {"id":"model","category":"model","currentValue":"gemini-4-argon","options":[
                    {"value":"gemini-4-argon","name":"Gemini 4 Argon"},{"value":"gemini-4-hydrogen","name":"Gemini 4 Hydrogen"},
                    {"value":"gpt-oss-120b-medium","name":"GPT-OSS 120B (Medium)"}]},
                {"id":"thought_level","category":"thought_level","currentValue":"medium","options":[
                    {"value":"low"},{"value":"medium"},{"value":"high"}]}]});
        let models = antigravity_models(&r);
        let ids: Vec<_> = models.iter().map(|m| (m.id.as_str(), m.efforts.join(","))).collect();
        assert_eq!(ids, [("gemini-4-argon", "low,medium,high".into()), ("gemini-4-hydrogen", "low,medium,high,xhigh".into()), ("gpt-oss-120b-medium", String::new())]);
        assert_eq!(models[0].default_effort, "medium");

        // The level is set once the tool reports another one, and again after a model switch resets it
        let mut st = State { session_id: Some("s".into()), effort: Some("high".into()), ..Default::default() };
        read_thought_level(&mut st, &r);
        let m = next_effort(&mut st).unwrap();
        assert_eq!((m["params"]["configId"].as_str(), m["params"]["value"].as_str()), (Some("thought_level"), Some("high")));
        assert!(next_effort(&mut st).is_none());
        st.pending.clear();
        read_thought_level(&mut st, &r);
        assert!(next_effort(&mut st).is_some());
        st.pending.clear();
        st.effort = Some("xhigh".into());
        assert!(next_effort(&mut st).is_none(), "not offered by this model");

        let text = "\n⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 4h9m8s.\n";
        assert_eq!(antigravity_limit(text), Some(((4 * 60 + 9) * 60 + 8) * 1000));
        assert_eq!(antigravity_limit("Quota reached. Resets in 2d3h."), Some((2 * 24 + 3) * 3_600_000));
        assert_eq!(antigravity_limit("all good"), None);
    }

    #[test]
    fn antigravity_subagent_tracking_and_wakeup() {
        use super::*;
        let root = std::env::temp_dir().join(format!("yi-agy-sub-{}", uuid::Uuid::new_v4()));
        let parent_id = "parent-conv";
        let sub_id = "sub-conv-1";
        let write_steps = |conv: &str, lines: &[Value]| {
            let dir = root.join(conv).join(".system_generated").join("logs");
            std::fs::create_dir_all(&dir).unwrap();
            let body = lines
                .iter()
                .map(|v| serde_json::to_string(v).unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(dir.join("transcript_full.jsonl"), format!("{body}\n")).unwrap();
        };

        // 1. Test parsing Created subagent id and cleaned output
        let generic_spawn = "Created At: x\nCompleted At: y\nCreated the following subagent:\n{\n  \"conversationId\":  \"sub-conv-1\",\n  \"logAbsoluteUri\":  \"file:///tmp/x\"\n}\n";
        assert_eq!(extract_created_subagent_ids(generic_spawn), vec!["sub-conv-1"]);
        assert!(is_agy_subagent_tool("invoke_subagent", &json!({"Role":"Researcher"})));
        assert!(is_agy_subagent_tool("send_message", &json!({"Recipient":"sub-conv-1","Message":"hi"})));
        assert!(!is_agy_subagent_tool("send_message", &json!({"Message":"hi"})));

        let media = vec![json!({"mime_type":"image/png","uri":"file:///tmp/out.png"})];
        assert_eq!(
            clean_agy_generic_output("Created At: a\nCompleted At: b\nDone!", Some(&media)),
            "Done!\n/tmp/out.png"
        );

        // 2. Simulate parent spawning subagent and going IDLE before subagent finishes
        let (hub, em, _rx) = crate::hub::test_emitter();
        let mut st = State {
            is_antigravity: true,
            session_id: Some(parent_id.into()),
            ..Default::default()
        };
        update(
            &mut st,
            &em,
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-sub-1",
                "kind": "other",
                "title": "invoke_subagent",
                "rawInput": {"TypeName":"image-generator","Role":"Image Generator","Prompt":"Draw a cat"}
            }),
        );
        update(
            &mut st,
            &em,
            &json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "tc-sub-1",
                "status": "completed"
            }),
        );
        // ToolResult must be deferred while subagent is pending
        assert!(st.agy_pending_calls.contains_key("tc-sub-1"));
        assert!(!hub.store.events_since("s", 0, 100).unwrap().iter().any(|e| matches!(e.body, EventBody::ToolResult { .. })));

        write_steps(
            parent_id,
            &[
                json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"Draw a cat"}),
                json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"invoke_subagent","args":{"TypeName":"image-generator","Role":"Image Generator","Prompt":"Draw a cat"}}]}),
                json!({"step_index":2,"source":"MODEL","type":"GENERIC","status":"DONE","content":generic_spawn}),
                json!({"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"Working on it, please wait."}),
            ],
        );
        write_steps(
            sub_id,
            &[
                json!({"step_index":0,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>sender=parent-conv</SYSTEM_MESSAGE>"}),
                json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"generate_image","args":{"ImageName":"cat.png","Prompt":"cat"}}]}),
            ],
        );

        let mut watch = AgyWatch::new(
            parent_id.into(),
            root.clone(),
            0,
            4,
            Some("end_turn".into()),
            std::mem::take(&mut st.agy_pending_calls),
            HashMap::new(),
            true,
        );
        let spec = agy_test_spec();
        let mut next_id = 10;
        let mut pending = HashMap::new();

        watch.detached = true;
        // First tick: discovers subagent and emits subagent's live generate_image ToolCall
        assert!(matches!(step_agy_watch(&mut watch, &spec, &em, &mut next_id, &mut pending), AgyStep::Wait));
        assert_eq!(watch.subagent_order, vec![sub_id]);
        // Listed as a background task named after the call's role; its own tool calls don't reopen the turn
        let tasks = hub.info("s").unwrap().background_tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!((tasks[0].id.as_str(), tasks[0].description.as_str()), (sub_id, "Image Generator"));
        assert!(!watch.reopened);

        // 3. Subagent completes generate_image and sends result back to parent
        write_steps(
            sub_id,
            &[
                json!({"step_index":0,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>sender=parent-conv</SYSTEM_MESSAGE>"}),
                json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"generate_image","args":{"ImageName":"cat.png","Prompt":"cat"}}]}),
                json!({"step_index":2,"source":"MODEL","type":"GENERIC","status":"DONE","content":"Created At: x\nCompleted At: y\nSaved cat.png","media":[{"mime_type":"image/png","uri":"file:///tmp/cat.png"}]}),
                json!({"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"send_message","args":{"Recipient":"parent-conv","Message":"Image saved at /tmp/cat.png"}}]}),
                json!({"step_index":4,"source":"MODEL","type":"GENERIC","status":"DONE","content":"Message sent to \"parent-conv\"."}),
                json!({"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"Done."}),
            ],
        );
        // Parent wakes up and writes final response
        write_steps(
            parent_id,
            &[
                json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"Draw a cat"}),
                json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"invoke_subagent","args":{"TypeName":"image-generator","Role":"Image Generator","Prompt":"Draw a cat"}}]}),
                json!({"step_index":2,"source":"MODEL","type":"GENERIC","status":"DONE","content":generic_spawn}),
                json!({"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"Working on it, please wait."}),
                json!({"step_index":4,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] sender=sub-conv-1 content=Image saved at /tmp/cat.png\n</SYSTEM_MESSAGE>"}),
                json!({"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"Here is your image: ![cat](/tmp/cat.png)"}),
            ],
        );

        assert!(matches!(step_agy_watch(&mut watch, &spec, &em, &mut next_id, &mut pending), AgyStep::Wait));
        let AgyStep::Done(Some(sync)) = step_agy_watch(&mut watch, &spec, &em, &mut next_id, &mut pending) else {
            panic!("should emit session/resume");
        };
        assert_eq!(sync["method"], "session/resume");
        assert_eq!(sync["params"]["sessionId"], parent_id);
        // The parent's wakeup reopened the session; the task list is empty again
        assert!(watch.reopened);
        assert!(hub.info("s").unwrap().background_tasks.is_empty());

        let evs = hub.store.events_since("s", 0, 100).unwrap();
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::ToolCall { name, .. } if name == "generate_image")));
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::ToolResult { id, output, .. } if id.starts_with("agy-sub:") && output.contains("/tmp/cat.png"))));
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::ToolResult { id, output, .. } if id == "tc-sub-1" && output == "Image saved at /tmp/cat.png")));
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::AssistantText { text, .. } if text == "Here is your image: ![cat](/tmp/cat.png)")));
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::System { text } if text == "后台任务完成：子 agent Image Generator")));
        // The finished line comes before the wakeup reply
        let done = evs.iter().position(|e| matches!(&e.body, EventBody::System { .. })).unwrap();
        let reply = evs.iter().position(|e| matches!(&e.body, EventBody::AssistantText { text, .. } if text.starts_with("Here is"))).unwrap();
        assert!(done < reply);

        std::fs::remove_dir_all(root).unwrap();
    }

    fn agy_test_spec() -> StartSpec {
        StartSpec {
            tool: ToolKind::Antigravity,
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
            program: "agy".into(),
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
        }
    }

    fn write_agy_steps(root: &std::path::Path, conv: &str, lines: &[Value]) {
        let dir = root.join(conv).join(".system_generated").join("logs");
        std::fs::create_dir_all(&dir).unwrap();
        let body = lines.iter().map(|v| serde_json::to_string(v).unwrap()).collect::<Vec<_>>().join("\n");
        std::fs::write(dir.join("transcript_full.jsonl"), format!("{body}\n")).unwrap();
    }

    #[test]
    fn antigravity_bypass_skips_permissions() {
        use super::*;
        let mut spec = agy_test_spec();
        assert!(!build_args(&spec).contains(&"--dangerously-skip-permissions".to_string()));
        spec.permission_mode = "bypass".into();
        assert_eq!(build_args(&spec), vec!["acp", "--dangerously-skip-permissions"]);
    }

    #[test]
    fn antigravity_background_command_result() {
        use super::*;
        let msg = "The following is a <SYSTEM_MESSAGE> not actually sent by the user.\n\n<SYSTEM_MESSAGE>\n[Message] timestamp=x sender=s1/task-2 priority=MESSAGE_PRIORITY_HIGH content=Task id \"s1/task-2\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nSUB-DONE-42\r\n\nLog: file:///tmp/task-2.log\n</SYSTEM_MESSAGE>";
        assert_eq!(agy_task_finished(msg), Some(("s1/task-2", "SUB-DONE-42".to_string())));
        assert!(!is_agy_wakeup(&json!({"type":"SYSTEM_MESSAGE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=x sender=system priority=MESSAGE_PRIORITY_HIGH content=settings changed\n</SYSTEM_MESSAGE>"})));
        assert!(is_agy_wakeup(&json!({"type":"SYSTEM_MESSAGE","content":msg})));
        assert_eq!(agy_background_task("Created At: x\nTool is running as a background task with task id: s1/task-2\nTask Description: sleep"), Some("s1/task-2"));

        let root = std::env::temp_dir().join(format!("yi-agy-bg-{}", uuid::Uuid::new_v4()));
        let (hub, em, _rx) = crate::hub::test_emitter();
        let spawn = "Created the following subagent:\n{\"conversationId\": \"s1\"}";
        let parent = vec![
            json!({"type":"USER_INPUT","status":"DONE","content":"go"}),
            json!({"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"invoke_subagent","args":{"Prompt":"run it"}}]}),
            json!({"type":"GENERIC","status":"DONE","content":spawn}),
            json!({"type":"PLANNER_RESPONSE","status":"DONE","content":"Waiting."}),
        ];
        write_agy_steps(&root, "p1", &parent);
        let mut sub = vec![
            json!({"type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>sender=p1 content=run it</SYSTEM_MESSAGE>"}),
            json!({"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{"CommandLine":"sleep 20; echo SUB-DONE-42"}}]}),
            json!({"type":"GENERIC","status":"RUNNING","content":"Created At: x\nTool is running as a background task with task id: s1/task-2\n"}),
            json!({"type":"PLANNER_RESPONSE","status":"DONE","content":"Running in the background, waiting."}),
        ];
        write_agy_steps(&root, "s1", &sub);
        let calls = HashMap::from([("tc".to_string(), AgyPendingCall { name: "invoke_subagent".into(), target_subagent: None, desc: String::new() })]);
        let mut watch = AgyWatch::new("p1".into(), root.clone(), 0, 4, Some("end_turn".into()), calls, HashMap::new(), true);
        let spec = agy_test_spec();
        let (mut next_id, mut pending) = (1, HashMap::new());
        // The background command keeps the subagent running, though its last reply has no tool calls
        assert!(matches!(step_agy_watch(&mut watch, &spec, &em, &mut next_id, &mut pending), AgyStep::Wait));
        assert!(!watch.subagents["s1"].done);
        sub.push(json!({"type":"SYSTEM_MESSAGE","status":"DONE","content":msg}));
        sub.push(json!({"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"send_message","args":{"Recipient":"p1","Message":"SUB-DONE-42"}}]}));
        sub.push(json!({"type":"GENERIC","status":"DONE","content":"Message sent to \"p1\"."}));
        write_agy_steps(&root, "s1", &sub);
        assert!(matches!(step_agy_watch(&mut watch, &spec, &em, &mut next_id, &mut pending), AgyStep::Wait));
        assert!(watch.subagents["s1"].done);
        let evs = hub.store.events_since("s", 0, 100).unwrap();
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::ToolResult { id, output, .. } if id == "agy-sub:s1:1:0" && output == "SUB-DONE-42")));
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::ToolResult { id, output, .. } if id == "tc" && output == "SUB-DONE-42")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn antigravity_subagent_waiting_for_approval() {
        use super::*;
        let root = std::env::temp_dir().join(format!("yi-agy-stall-{}", uuid::Uuid::new_v4()));
        let (hub, em, _rx) = crate::hub::test_emitter();
        write_agy_steps(&root, "p1", &[json!({"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"send_message","args":{"Recipient":"s1","Message":"run"}}]})]);
        write_agy_steps(&root, "s1", &[
            json!({"type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>sender=p1 content=run</SYSTEM_MESSAGE>"}),
            json!({"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{"CommandLine":"echo hi"}}]}),
        ]);
        let spec = agy_test_spec();
        let (mut next_id, mut pending) = (1, HashMap::new());
        let errors = || hub.store.events_since("s", 0, 100).unwrap().iter().filter(|e| matches!(&e.body, EventBody::Error { message } if message.starts_with("子 agent 在等待审批"))).count();
        for stall_check in [false, true] {
            let mut watch = AgyWatch::new("p1".into(), root.clone(), 0, 1, None, HashMap::new(), HashMap::from([("s1".to_string(), 0)]), stall_check);
            watch.detached = true;
            assert!(matches!(step_agy_watch(&mut watch, &spec, &em, &mut next_id, &mut pending), AgyStep::Wait));
            // A running subagent is a background task
            let tasks = hub.info("s").unwrap().background_tasks;
            assert_eq!(tasks.len(), 1);
            assert_eq!((tasks[0].id.as_str(), tasks[0].kind.as_str()), ("s1", "local_agent"));
            // The subagent's transcript hasn't moved for longer than the stall limit: said once, and the watch goes on
            // (allow-all runs agy with --dangerously-skip-permissions, where waiting means a slow tool)
            watch.subagents.get_mut("s1").unwrap().transcript.changed_at = Some(tokio::time::Instant::now() - std::time::Duration::from_secs(31));
            assert!(matches!(step_agy_watch(&mut watch, &spec, &em, &mut next_id, &mut pending), AgyStep::Wait));
            assert!(matches!(step_agy_watch(&mut watch, &spec, &em, &mut next_id, &mut pending), AgyStep::Wait));
            assert_eq!(errors(), stall_check as usize);
            // Never reopened by the subagent's own events
            assert!(!watch.reopened);
            if stall_check {
                // Stopped from the background task list
                assert!(watch.stop("s1"));
                assert!(!watch.stop("s1"));
                watch.flush_out(&em);
                assert!(watch.tasks().is_empty());
                let evs = hub.store.events_since("s", 0, 100).unwrap();
                assert!(evs.iter().any(|e| matches!(&e.body, EventBody::System { text } if text == "后台任务已停止：子 agent s1")));
            }
        }
        assert_eq!(count_agy_transcript_steps(&root, "s1"), 2);
        assert_eq!(count_agy_transcript_steps(&root, "../s1"), 0);
        std::fs::remove_dir_all(root).unwrap();
    }
}
