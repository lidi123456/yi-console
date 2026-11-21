//! ACP (Agent Client Protocol) adapter, for `cursor-agent acp`, `kiro-cli acp`, `qodercli --acp`, `codebuddy --acp`.
//! Protocol: JSON-RPC 2.0 over stdio; session/update notifications push content, session/request_permission asks for approval.

use super::{qoder_tasks, AdapterCmd, JsonProc, Prompt, StartSpec};
use crate::hub::Emitter;
use crate::util::now_ms;
use yi_proto::*;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Pending {
    Init,
    Auth,
    Session,
    /// session/resume: attach to an existing Antigravity session without replaying history; falls back to session/load
    Resume,
    /// session/load: switch to session/new when the tool can't find this session
    Load,
    Prompt,
    SetModel,
    SetConfig,
    /// Antigravity's `thought_level` option
    SetEffort,
    AgyResume,
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
    /// Outside an Antigravity prompt, the transcript watch publishes this call
    agy_detached: bool,
}

#[derive(Debug, Clone)]
struct PendingElicit {
    rpc_id: Value,
    fields: Vec<ElicitField>,
}

#[derive(Debug, Clone)]
enum ElicitField {
    /// Antigravity's `ask_question` form (`qN` for options, `qN_other` for free-text)
    AskQuestion {
        question_id: String,
        select_key: Option<String>,
        other_key: Option<String>,
        multi: bool,
    },
    /// Generic ACP/MCP `elicitation/create` form property
    Property {
        question_id: String,
        key: String,
        kind: ElicitPropKind,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElicitPropKind {
    String,
    MultiArray,
    Bool,
    Integer,
    Number,
}

#[derive(Default)]
struct State {
    qoder_config_dir: Option<PathBuf>,
    /// The computer use notice still has to go out with the next prompt (once per launch)
    desktop_notice: bool,
    /// The same for the AI browser (yi_browser)
    browser_notice: bool,
    /// The session's appended system prompt still has to go out with the next prompt (new tool session or changed)
    system_notice: Option<String>,
    /// Host personal memory L0 snapshot still to go out with the next prompt (new tool session or after compaction)
    memory_notice: Option<String>,
    /// True when this launch carries `yi_memory` (so compaction / load fallback should re-arm `memory_notice`)
    memory_enabled: bool,
    /// Compact cross-account handoff summary injected if `session/load` fell back to a fresh ACP session
    account_handoff: Option<String>,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    session_id: Option<String>,
    prompting: bool,
    queue: Vec<Prompt>,
    /// initialize declared support for image input
    image_ok: bool,
    /// initialize declared support for session/resume (attach without history replay)
    can_resume: bool,
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
    /// Antigravity: models reported by the session (with each model's supported thinking levels) and the active model id
    agy_models: Vec<ModelOption>,
    agy_model: Option<String>,
    /// Levels the tool offers for the current model, and the one it is on
    thought_levels: Vec<String>,
    thought_current: Option<String>,
    /// Text of the turn's last reply (Antigravity reports a used-up quota only as reply text)
    last_reply: String,
    /// Cursor's cursor/ask_question requests: request id -> (JSON-RPC id)
    asks: HashMap<String, Value>,
    /// ACP `elicitation/create` form requests (Antigravity's `ask_question` and MCP form elicitations)
    elicits: HashMap<String, PendingElicit>,
    elicit_seq: u64,
    /// Auth methods returned by initialize. We don't call authenticate up front (cursor_login opens a browser,
    /// and with CURSOR_API_KEY set it isn't needed at all). Cursor authentication failures must be
    /// surfaced instead of starting a browser login on a headless host; other tools retry once.
    auth_method: Option<String>,
    auth_tried: bool,
    /// This run continues an existing session (session/load was sent) rather than a new one; reset to false when load fails and we fall back to a new session
    resumed: bool,
    /// Kiro compacts in the background after its /compact turn has ended, and a prompt sent meanwhile never gets
    /// an answer. Between `_kiro.dev/compaction/status` started and completed, messages wait in `queue`
    compacting: Option<i64>,
    /// The last prompt we sent was /compact, so the next compaction is manual
    compact_manual: bool,
    /// Cursor has no compaction over ACP; /compact is emulated (`cursor_compact_start`). `compacting` holds the queue meanwhile
    is_cursor: bool,
    cursor_compact: Option<CursorCompact>,
    /// Antigravity (`agy acp`): tracks asynchronous subagents and background tasks spawned in the current turn
    is_antigravity: bool,
    agy_pre_turn_steps: usize,
    /// Active manual `/compact` turn on Antigravity (confirmed via a `CHECKPOINT` step in the transcript)
    agy_compact: Option<AgyCompact>,
    /// Last token count reported by ACP `usage_update` on Antigravity
    agy_context_tokens: Option<u64>,
    /// Transcript step indices of `CHECKPOINT` steps already turned into `EventBody::Compact`
    agy_seen_checkpoints: HashSet<usize>,
    /// First possible wakeup step after the prompt or the last finished wakeup
    agy_wake_from: usize,
    /// Parent transcript step count known to be at or past agy's own ACP stream cursor
    agy_last_streamed_steps: usize,
    agy_pending_calls: HashMap<String, AgyPendingCall>,
    agy_send_baselines: HashMap<String, usize>,
    agy_open_bg_calls: VecDeque<(String, String)>,
    agy_bg_tasks: Vec<AgyBgTask>,
    agy_parent_transcript: AgyTranscript,
    agy_seen_system_steps: HashSet<usize>,
    agy_watch: Option<AgyWatch>,
    /// Subagents / background tasks still running when a new prompt went out: the prompt's own watch picks them up when it returns
    agy_carry: AgyCarry,
    /// Tool call ids suppressed during muted `session/load` replay (never re-emit if flushed afterwards)
    muted_tools: HashSet<String>,
    /// Active pre-turn replay guard during an Antigravity `session/prompt`
    agy_replay: Option<AgyReplayGuard>,
    agy_replay_hidden: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum AgyWakeReplayItem {
    Thinking(String),
    Assistant(String),
    Tool { name: String, input: Value },
}

#[derive(Debug, Clone, PartialEq)]
enum AgyHistoryItem {
    User(String),
    Thinking(String),
    Assistant(String),
    Tool { name: String, input: Value },
}

#[derive(Debug, Clone, PartialEq)]
enum AgyReplayAction {
    Drop,
    StripPrefix(String),
    Pass,
}

#[derive(Debug, Clone, Default)]
struct AgyReplayGuard {
    /// Trimmed user prompt text for the active turn; used to detect agy's live `user_message_chunk` echo
    prompt_text: String,
    /// Wakeup steps already streamed by `AgyWatch` before `session/resume` ran
    unresumed: VecDeque<AgyWakeReplayItem>,
    /// Pre-turn transcript items (`steps[from..pre_turn_steps]`) that agy could replay before answering
    history: Vec<AgyHistoryItem>,
    /// Independent monotonic cursors per kind so concatenated thoughts/replies don't skip interleaved tool calls
    thought_cursor: usize,
    tool_cursor: usize,
    /// Consumed historical tool indices; lets out-of-order parallel tool calls match once even behind `tool_cursor`
    used_tools: HashSet<usize>,
    assistant_cursor: usize,
    user_cursor: usize,
    /// Highest history index (+1) matched by any non-user item so far (anchors historical USER_INPUT lookup)
    max_consumed: usize,
    /// True once at least one pre-turn item has been matched and suppressed in this prompt
    replaying: bool,
    /// Accumulated `user_message_chunk` buffer for the current echoed user message
    user_buf: String,
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

    fn ready_to_prompt(&self) -> bool {
        self.session_id.is_some()
            && !config_pending(self)
            && !self.prompting
            && !self.loading
            && self.compacting.is_none()
            && !self.pending.values().any(|p| matches!(p, Pending::AgyResume))
            && self.agy_watch.as_ref().is_none_or(|watch| !watch.reopened)
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
    if spec.tool == ToolKind::Kiro && spec.permission_mode != "bypass" {
        // Trust only the gateway servers the user turned on; their live grants still gate every call
        let has = |name: &str| spec.mcp_servers.iter().any(|server| server["name"] == name);
        let mut rules = vec![];
        if has(crate::mcp::DESKTOP_SERVER) {
            rules.extend(crate::computer_use::tool_names().map(|name| format!("@{}/{name}", crate::mcp::DESKTOP_SERVER)));
        }
        if has(crate::mcp::BROWSER_SERVER) {
            rules.extend(yi_browser::tool_names().map(|name| format!("@{}/{name}", crate::mcp::BROWSER_SERVER)));
        }
        if has(crate::mcp::MEMORY_SERVER) {
            rules.extend(crate::memory::tool_names().map(|name| format!("@{}/{name}", crate::mcp::MEMORY_SERVER)));
        }
        if has(crate::mcp::SESSION_SERVER) {
            rules.extend(crate::session_mcp::tool_names().map(|name| format!("@{}/{name}", crate::mcp::SESSION_SERVER)));
        }
        if !rules.is_empty() {
            let rules = rules.join(",");
            if let Some(index) = a.iter().rposition(|arg| arg == "--trust-tools" || arg.starts_with("--trust-tools=")) {
                if a[index].contains('=') { a[index].push(','); a[index].push_str(&rules); }
                else if let Some(value) = a.get_mut(index + 1) { value.push(','); value.push_str(&rules); }
            } else { a.extend(["--trust-tools".into(), rules]); }
        }
    }
    a
}

pub async fn run(spec: StartSpec, em: Emitter, mut rx: mpsc::UnboundedReceiver<AdapterCmd>) {
    let args = build_args(&spec);
    let mut p = match JsonProc::spawn_with_env(&spec.program, &args, &spec.cwd, &spec.env, &spec.env_unset) {
        Ok(p) => p,
        Err(e) => return em.exited(Some(format!("启动 {} 失败: {e}", spec.tool.as_str()))),
    };
    let mut st = State {
        qoder_config_dir: (spec.tool == ToolKind::Qoder).then(|| qoder_tasks::config_dir(&spec.cwd, &spec.env, &spec.env_unset)),
        perm: spec.permission_mode.clone(),
        native_modes: spec.tool == ToolKind::Antigravity,
        is_antigravity: spec.tool == ToolKind::Antigravity,
        is_cursor: spec.tool == ToolKind::Cursor,
        effort: spec.effort.clone().filter(|e| !e.is_empty() && spec.tool == ToolKind::Antigravity),
        desktop_notice: spec.mcp_servers.iter().any(|server| server["name"] == crate::mcp::DESKTOP_SERVER),
        browser_notice: spec.mcp_servers.iter().any(|server| server["name"] == crate::mcp::BROWSER_SERVER),
        system_notice: super::acp_system_notice(&spec),
        memory_notice: super::acp_memory_notice(&spec),
        memory_enabled: spec.memory_prompt.is_some()
            || spec.mcp_servers.iter().any(|server| server["name"] == crate::mcp::MEMORY_SERVER),
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
    let mut qoder_tick = tokio::time::interval(std::time::Duration::from_secs(1));
    qoder_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let startup_timeout = tokio::time::sleep(std::time::Duration::from_secs(60));
    tokio::pin!(startup_timeout);
    loop {
        tokio::select! {
            _ = &mut startup_timeout, if spec.tool == ToolKind::Cursor && st.session_id.is_none() => {
                fatal = Some("Cursor 启动超时（60 秒未创建会话），请检查账号登录状态和网络后重试".into());
                killed = true;
                break;
            }
            line = p.stdout.next_line() => {
                let Ok(Some(line)) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                match handle(&mut st, &spec, &v, &em) {
                    Ok(outs) => for out in outs { let _ = p.send(&out).await; },
                    Err(msg) => { fatal = Some(msg); killed = true; break; }
                }
                if let Some(m) = next_config(&mut st) { let _ = p.send(&m).await; }
                if let Some(m) = next_effort(&mut st) { let _ = p.send(&m).await; }
                if st.ready_to_prompt() && !st.queue.is_empty() {
                    let next = st.queue.remove(0);
                    em.status(SessionStatus::Running, None);
                    let m = prompt(&mut st, &em, &next);
                    let _ = p.send(&m).await;
                }
            }
            _ = agy_tick.tick(), if st.agy_watch.is_some() || (st.is_antigravity && (!st.agy_open_bg_calls.is_empty() || !st.agy_bg_tasks.is_empty() || !st.agy_carry.bg_tasks.is_empty())) => {
                if st.agy_watch.is_some() {
                    if let Some(m) = poll_agy_watch(&mut st, &spec, &em) {
                        let _ = p.send(&m).await;
                    }
                } else {
                    poll_agy_prompt_bg(&mut st, &em);
                }
                if st.ready_to_prompt() && !st.queue.is_empty() {
                    let next = st.queue.remove(0);
                    em.status(SessionStatus::Running, None);
                    let m = prompt(&mut st, &em, &next);
                    let _ = p.send(&m).await;
                }
            }
            // Task notifications can wake Qoder after the ACP prompt has returned. Its task files still change.
            _ = qoder_tick.tick(), if st.qoder_config_dir.is_some() && st.session_id.is_some() && !st.loading => {
                if let Some(dir) = &st.qoder_config_dir {
                    em.sync_qoder_plan(dir);
                }
            }
            cmd = rx.recv() => match cmd {
                Some(AdapterCmd::Send(next)) => {
                    em.status(SessionStatus::Running, None);
                    if retry_failed_model(&mut st) {
                        if let Some(m) = next_config(&mut st) { let _ = p.send(&m).await; }
                    }
                    if st.ready_to_prompt() {
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
                    if st.is_antigravity {
                        if let Some(opt) = st.agy_models.iter().find(|m| m.id == model) {
                            if st.effort.as_deref().is_some_and(|e| !opt.efforts.iter().any(|l| l == e)) {
                                st.effort = None;
                            }
                        }
                    }
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
                    if effort.is_empty() {
                        // Back to "default": clear the override and reset the current model once if it offers a default level
                        st.effort = None;
                        if let Some(m) = reset_effort_to_default(&mut st) { let _ = p.send(&m).await; }
                    } else {
                        st.effort = Some(effort);
                        if let Some(m) = next_effort(&mut st) { let _ = p.send(&m).await; }
                    }
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
                        if let Some(sid) = st.session_id.as_deref() {
                            st.agy_wake_from = count_agy_transcript_steps(&agy_brain_dir(), sid);
                        }
                        st.prompting = false;
                        st.end_subagents();
                        let keep_watch = !st.agy_carry.is_empty() && st.session_id.is_some();
                        if keep_watch {
                            if let Some(sid) = st.session_id.clone() {
                                let steps = st.agy_wake_from;
                                let mut w = AgyWatch::new(sid, agy_brain_dir(), steps, steps, None, HashMap::new(), HashMap::new(), st.perm != "bypass");
                                w.adopt(std::mem::take(&mut st.agy_carry));
                                w.detached = true;
                                st.agy_watch = Some(w);
                            }
                        }
                        em.watch_active(keep_watch);
                        em.event(EventBody::TurnEnd { stop_reason: Some("cancelled".into()), model: None });
                    }
                    if let Some(sid) = st.session_id.clone() {
                        for (rid, rpc_id) in st.perms.drain().chain(st.asks.drain()) {
                            let _ = p.send(&json!({"jsonrpc":"2.0","id":rpc_id,
                                "result":{"outcome":{"outcome":"cancelled"}}})).await;
                            em.event(EventBody::PermissionResolved { request_id: rid, option_id: "cancelled".into(), answers: vec![] });
                        }
                        for (rid, el) in st.elicits.drain() {
                            let _ = p.send(&elicit_response(&el, "cancelled", &[])).await;
                            em.event(EventBody::PermissionResolved { request_id: rid, option_id: "cancelled".into(), answers: vec![] });
                        }
                        let _ = p.send(&json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":sid}})).await;
                    }
                }
                Some(AdapterCmd::Permission { request_id, option_id, answers }) => {
                    if let Some(el) = st.elicits.remove(&request_id) {
                        let _ = p.send(&elicit_response(&el, &option_id, &answers)).await;
                        let resolved_answers = if option_id == "cancelled" { vec![] } else { answers };
                        em.event(EventBody::PermissionResolved { request_id, option_id, answers: resolved_answers });
                    } else if let Some(rpc_id) = st.asks.remove(&request_id) {
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

/// Cursor only exposes individual model parameters when `parameterizedModelPicker` is declared;
/// Antigravity only sends `elicitation/create` for `ask_question` when `elicitation.form` is declared.
pub fn client_capabilities(tool: ToolKind) -> Value {
    let mut caps = json!({"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false});
    if tool == ToolKind::Cursor { caps["_meta"] = json!({"parameterizedModelPicker":true}); }
    if tool == ToolKind::Antigravity { caps["elicitation"] = json!({"form":{}}); }
    caps
}

fn config_pending(st: &State) -> bool {
    !st.config_queue.is_empty() || st.pending.values().any(|p| matches!(p, Pending::SetModel | Pending::SetConfig | Pending::SetEffort))
}

/// Antigravity: record the `thought_level` option from a configOptions list (absent: the model has no levels)
fn read_thought_level(st: &mut State, v: &Value) {
    let Some(options) = v["configOptions"].as_array() else { return };
    if let Some(model) = options.iter().find(|o| o["id"] == "model").and_then(|o| o["currentValue"].as_str()) {
        st.agy_model = Some(model.to_string());
    }
    let option = options.iter().find(|o| o["category"] == "thought_level");
    st.thought_levels = option.and_then(|o| o["options"].as_array()).into_iter().flatten()
        .filter_map(|o| o["value"].as_str().map(str::to_string)).collect();
    st.thought_current = option.and_then(|o| o["currentValue"].as_str()).map(str::to_string);
}

/// Antigravity: true unless the active model is known in `agy_models` and does not offer `effort`
fn agy_model_supports_effort(st: &State, effort: &str) -> bool {
    let Some(model) = st.agy_model.as_deref() else { return true };
    let Some(opt) = st.agy_models.iter().find(|m| m.id == model) else { return true };
    opt.efforts.iter().any(|e| e == effort)
}

/// Antigravity: one-shot reset back to the active model's default level when the user clears thinking level
fn reset_effort_to_default(st: &mut State) -> Option<Value> {
    if st.loading || st.session_id.is_none() || st.pending.values().any(|p| matches!(p, Pending::SetModel | Pending::SetEffort)) {
        return None;
    }
    let default = st
        .agy_model
        .as_deref()
        .and_then(|id| st.agy_models.iter().find(|m| m.id == id))
        .map(|m| m.default_effort.as_str())
        .filter(|d| !d.is_empty())
        .unwrap_or("medium")
        .to_string();
    if st.thought_current.as_deref() == Some(default.as_str())
        || !st.thought_levels.contains(&default)
        || !agy_model_supports_effort(st, &default)
    {
        return None;
    }
    st.thought_current = Some(default.clone());
    Some(st.req(Pending::SetEffort, "session/set_config_option",
        json!({"sessionId":st.session_id,"configId":"thought_level","value":default})))
}

/// Antigravity: set the chosen thinking level when the tool is on another one and the current model offers it
fn next_effort(st: &mut State) -> Option<Value> {
    let want = st.effort.clone()?;
    if st.loading || st.session_id.is_none() || st.pending.values().any(|p| matches!(p, Pending::SetModel | Pending::SetEffort))
        || st.thought_current.as_deref() == Some(want.as_str())
        || !st.thought_levels.contains(&want)
        || !agy_model_supports_effort(st, &want) {
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
    let brain_dir = st.is_antigravity.then(agy_brain_dir);
    prompt_with_brain_dir(st, em, p, brain_dir.as_deref())
}

fn prompt_with_brain_dir(st: &mut State, em: &Emitter, p: &Prompt, brain_dir: Option<&std::path::Path>) -> Value {
    st.prompting = true;
    st.last_reply.clear();
    st.compact_manual = is_compact_command(&p.text);
    st.agy_compact = None;
    st.turn_credits = 0.0;
    st.agy_pending_calls.clear();
    st.agy_send_baselines.clear();
    st.agy_open_bg_calls.clear();
    st.agy_bg_tasks.clear();
    st.agy_seen_system_steps.clear();
    // The watch can't run during a prompt (the parent's own steps would look like a wakeup): its running subagents
    // wait in agy_carry for this prompt's watch. A report arriving meanwhile is handed to this turn by agy itself
    carry_agy_watch(st, em);
    if st.is_antigravity {
        st.tools.retain(|_, tool| !tool.agy_detached);
    }
    st.agy_replay_hidden.clear();
    let unresumed = std::mem::take(&mut st.agy_carry.unresumed_items);
    let pre_steps = match (st.session_id.as_deref(), brain_dir) {
        (Some(sid), Some(dir)) if st.is_antigravity => read_agy_transcript_steps(dir, sid),
        _ => vec![],
    };
    st.agy_pre_turn_steps = pre_steps.len();
    st.agy_wake_from = st.agy_pre_turn_steps;
    st.agy_replay = if st.is_antigravity {
        let history = if st.agy_last_streamed_steps < pre_steps.len() {
            extract_agy_history(&pre_steps[st.agy_last_streamed_steps..])
        } else {
            vec![]
        };
        AgyReplayGuard::new(&p.text, unresumed, history)
    } else {
        None
    };
    let sid = st.session_id.clone().unwrap_or_default();
    if st.is_cursor {
        if let Some((prior, extra)) = split_compact(&p.text) {
            return cursor_compact_start(st, em, &prior, extra);
        }
    }
    if st.is_antigravity && st.compact_manual && !pre_steps.is_empty() {
        let pre_tokens = agy_last_step_tokens(&pre_steps).or(st.agy_context_tokens);
        st.agy_compact = Some(AgyCompact { started: now_ms(), pre_tokens });
        em.event(EventBody::Compact {
            phase: "start".into(),
            trigger: Some("manual".into()),
            pre_tokens: None,
            post_tokens: None,
            duration_ms: None,
        });
    }
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
        if let Some(mem) = st.memory_notice.take() {
            blocks.push(json!({"type":"text","text":mem}));
        }
        if let Some(notice) = st.system_notice.take() {
            blocks.push(json!({"type":"text","text":notice}));
        }
        if let Some(handoff) = st.account_handoff.take() {
            blocks.push(json!({"type":"text","text":handoff}));
        }
    }
    if st.desktop_notice && !p.text.trim_start().starts_with('/') {
        st.desktop_notice = false;
        blocks.push(json!({"type":"text","text":crate::computer_use::acp_notice()}));
    }
    if st.browser_notice && !p.text.trim_start().starts_with('/') {
        st.browser_notice = false;
        blocks.push(json!({"type":"text","text":crate::browser::acp_notice()}));
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
    // Clear the previous model's thinking levels so next_effort waits for the new model's configOptions
    st.thought_levels.clear();
    st.thought_current = None;
    st.req(
        Pending::SetModel,
        "session/set_model",
        json!({"sessionId":sid,"modelId":model}),
    )
}

fn extract_agy_user_text(raw: &str) -> String {
    if let Some((_, rest)) = raw.split_once("<USER_REQUEST>") {
        if let Some((inner, _)) = rest.split_once("</USER_REQUEST>") {
            return inner.trim().to_string();
        }
    }
    raw.trim().to_string()
}

fn extract_agy_history(steps: &[Value]) -> Vec<AgyHistoryItem> {
    let mut items = vec![];
    for step in steps {
        match step["type"].as_str().unwrap_or("") {
            "USER_INPUT" => {
                let text = extract_agy_user_text(step["content"].as_str().unwrap_or(""));
                if !text.is_empty() {
                    items.push(AgyHistoryItem::User(text));
                }
            }
            "PLANNER_RESPONSE" => {
                if let Some(thinking) = step["thinking"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    items.push(AgyHistoryItem::Thinking(thinking.to_string()));
                }
                for tc in step["tool_calls"].as_array().into_iter().flatten() {
                    let name = tc["name"].as_str().unwrap_or("").to_string();
                    if !name.is_empty() {
                        items.push(AgyHistoryItem::Tool { name, input: tc["args"].clone() });
                    }
                }
                if let Some(content) = step["content"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    items.push(AgyHistoryItem::Assistant(content.to_string()));
                }
            }
            "ERROR_MESSAGE" => {
                if let Some(raw) = step["content"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    let clean = raw.strip_prefix("Error:").map(str::trim).unwrap_or(raw);
                    if !clean.is_empty() {
                        items.push(AgyHistoryItem::Assistant(clean.to_string()));
                    }
                }
            }
            _ => {}
        }
    }
    items
}

fn strip_agy_warning_banner(rem: &str) -> Option<&str> {
    let after_mark = rem.strip_prefix('⚠')?;
    // A warning banner runs until a newline, the next '⚠', or the end of the segment.
    let end = after_mark
        .find(['\n', '⚠'])
        .map(|i| '⚠'.len_utf8() + i)
        .unwrap_or(rem.len());
    Some(rem[end..].trim_start())
}

fn user_echo_matches(echoed: &str, prompt_text: &str) -> bool {
    let echoed = extract_agy_user_text(echoed);
    if echoed.is_empty() {
        return false;
    }
    if prompt_text.is_empty() {
        return true;
    }
    echoed == prompt_text || echoed.ends_with(prompt_text)
}

fn consume_agy_unresumed(prefix: &mut VecDeque<AgyWakeReplayItem>, matches: impl Fn(&AgyWakeReplayItem) -> bool) -> bool {
    if prefix.is_empty() {
        return false;
    }
    if prefix.front().is_some_and(&matches) {
        prefix.pop_front();
        return true;
    }
    // agy may skip Thinking steps when replaying inside session/prompt
    let skip_thinking = prefix.iter().take_while(|it| matches!(it, AgyWakeReplayItem::Thinking(_))).count();
    if skip_thinking > 0 && prefix.get(skip_thinking).is_some_and(&matches) {
        prefix.drain(..=skip_thinking);
        return true;
    }
    false
}

impl AgyReplayGuard {
    fn new(prompt_text: &str, unresumed: VecDeque<AgyWakeReplayItem>, history: Vec<AgyHistoryItem>) -> Option<Self> {
        if unresumed.is_empty() && history.is_empty() {
            return None;
        }
        Some(Self {
            prompt_text: prompt_text.trim().to_string(),
            unresumed,
            history,
            thought_cursor: 0,
            tool_cursor: 0,
            used_tools: HashSet::new(),
            assistant_cursor: 0,
            user_cursor: 0,
            max_consumed: 0,
            replaying: false,
            user_buf: String::new(),
        })
    }

    fn text_cursor(&self, seg: Seg) -> usize {
        match seg {
            Seg::Thought => self.thought_cursor,
            Seg::Agent => self.assistant_cursor,
            Seg::User => self.user_cursor,
        }
    }

    fn set_text_cursor(&mut self, seg: Seg, pos: usize) {
        match seg {
            Seg::Thought => self.thought_cursor = pos,
            Seg::Agent => self.assistant_cursor = pos,
            Seg::User => self.user_cursor = pos,
        }
        if seg != Seg::User {
            self.max_consumed = self.max_consumed.max(pos);
        }
    }

    /// Handle a `user_message_chunk` during an Antigravity live prompt. Returns `true` when this echo
    /// belongs to the current prompt and the replay guard should be disarmed.
    fn on_user_chunk(&mut self, text: &str) -> bool {
        self.user_buf.push_str(text);
        let echoed = extract_agy_user_text(&self.user_buf);
        if echoed.is_empty() {
            return false;
        }
        if !self.replaying {
            if user_echo_matches(&echoed, &self.prompt_text) {
                return true;
            }
            if let Some(off) = self.history[self.user_cursor..].iter().position(|it| matches!(it, AgyHistoryItem::User(u) if u == &echoed)) {
                let idx = self.user_cursor + off;
                if idx + 1 < self.history.len() {
                    self.user_cursor = idx + 1;
                    self.replaying = true;
                    self.user_buf.clear();
                }
            }
            return false;
        }
        // Inside an active multi-turn replay: only advance past an old USER_INPUT if non-user history still follows it.
        let start = self.user_cursor.max(self.max_consumed);
        if let Some(off) = self.history[start..].iter().position(|it| matches!(it, AgyHistoryItem::User(u) if u == &echoed)) {
            let idx = start + off;
            let has_following = self.history[idx + 1..].iter().any(|it| !matches!(it, AgyHistoryItem::User(_)));
            if has_following {
                self.user_cursor = idx + 1;
                self.user_buf.clear();
                return false;
            }
        }
        user_echo_matches(&echoed, &self.prompt_text)
    }

    fn clear_user_buf(&mut self) {
        self.user_buf.clear();
    }

    /// True if `buf` could still be a prefix of a pre-turn replay item (hold transient deltas until flush).
    fn might_match_prefix(&self, seg: Seg, buf: &str) -> bool {
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            return true;
        }
        if seg == Seg::Agent && self.replaying && trimmed.starts_with('⚠') {
            return true;
        }
        let check = |h: &str| -> bool {
            if h.is_empty() {
                return false;
            }
            if h.starts_with(trimmed) || trimmed.starts_with(h) {
                return true;
            }
            if seg == Seg::Agent {
                if let Some(after) = trimmed.strip_prefix('⚠').map(str::trim_start) {
                    return after.is_empty() || h.starts_with(after) || after.starts_with(h);
                }
            }
            false
        };
        if self.unresumed.iter().any(|it| match (seg, it) {
            (Seg::Thought, AgyWakeReplayItem::Thinking(h)) => check(h),
            (Seg::Agent, AgyWakeReplayItem::Assistant(h)) => check(h),
            _ => false,
        }) {
            return true;
        }
        let cursor = self.text_cursor(seg);
        self.history[cursor..].iter().any(|it| match (seg, it) {
            (Seg::Thought, AgyHistoryItem::Thinking(h)) => check(h),
            (Seg::Agent, AgyHistoryItem::Assistant(h)) => check(h),
            _ => false,
        })
    }

    fn consume_text(&mut self, seg: Seg, raw_text: &str) -> AgyReplayAction {
        self.user_buf.clear();
        let trimmed = raw_text.trim();
        if trimmed.is_empty() {
            return AgyReplayAction::Drop;
        }
        let matched_unresumed = match seg {
            Seg::Thought => consume_agy_unresumed(&mut self.unresumed, |it| matches!(it, AgyWakeReplayItem::Thinking(t) if t == trimmed)),
            Seg::Agent => consume_agy_unresumed(&mut self.unresumed, |it| matches!(it, AgyWakeReplayItem::Assistant(t) if t == trimmed)),
            Seg::User => false,
        };
        if matched_unresumed {
            self.replaying = true;
            let cursor = self.text_cursor(seg);
            if let Some(off) = self.history[cursor..].iter().position(|it| match (seg, it) {
                (Seg::Thought, AgyHistoryItem::Thinking(h)) => h == trimmed,
                (Seg::Agent, AgyHistoryItem::Assistant(h)) => h == trimmed,
                _ => false,
            }) {
                self.set_text_cursor(seg, cursor + off + 1);
            }
            return AgyReplayAction::Drop;
        }
        let mut rem = trimmed;
        let mut scan = self.text_cursor(seg);
        let mut matched_any = false;
        let mut clean_boundary = false;
        while !rem.is_empty() {
            let exact = self.history[scan..].iter().position(|it| match (seg, it) {
                (Seg::Thought, AgyHistoryItem::Thinking(h)) => rem == h,
                (Seg::Agent, AgyHistoryItem::Assistant(h)) => {
                    rem == h || rem.strip_prefix('⚠').is_some_and(|r| r.trim() == h)
                }
                _ => false,
            });
            if let Some(off) = exact {
                scan += off + 1;
                rem = "";
                matched_any = true;
                break;
            }
            let mut best: Option<(usize, usize)> = None;
            for (off, it) in self.history[scan..].iter().enumerate() {
                let h = match (seg, it) {
                    (Seg::Thought, AgyHistoryItem::Thinking(h)) => h.as_str(),
                    (Seg::Agent, AgyHistoryItem::Assistant(h)) => h.as_str(),
                    _ => continue,
                };
                if h.is_empty() {
                    continue;
                }
                let consumed = if rem.starts_with(h) {
                    Some(h.len())
                } else if seg == Seg::Agent {
                    rem.strip_prefix('⚠').and_then(|after| {
                        let t = after.trim_start();
                        t.starts_with(h).then(|| (rem.len() - t.len()) + h.len())
                    })
                } else {
                    None
                };
                if let Some(len) = consumed {
                    if best.is_none_or(|(_, best_len)| len > best_len) {
                        best = Some((scan + off, len));
                    }
                }
            }
            if let Some((idx, len)) = best {
                let after = &rem[len..];
                clean_boundary = after.is_empty() || after.starts_with('\n');
                scan = idx + 1;
                rem = after.trim_start();
                matched_any = true;
                continue;
            }
            if seg == Seg::Agent && (self.replaying || matched_any) {
                // Quota/error banners are stored in agy's SQLite DB but omitted from transcript_full.jsonl.
                if let Some(rest) = strip_agy_warning_banner(rem) {
                    clean_boundary = true;
                    rem = rest;
                    matched_any = true;
                    continue;
                }
            }
            break;
        }
        if matched_any {
            if rem.is_empty() {
                self.set_text_cursor(seg, scan);
                self.replaying = true;
                return AgyReplayAction::Drop;
            }
            if self.replaying && clean_boundary {
                self.set_text_cursor(seg, scan);
                return AgyReplayAction::StripPrefix(rem.to_string());
            }
        }
        AgyReplayAction::Pass
    }

    fn consume_tool(&mut self, name: &str, input: &Value) -> bool {
        self.user_buf.clear();
        let find_tool = |guard: &Self| {
            let matches = |i: usize| {
                !guard.used_tools.contains(&i)
                    && matches!(&guard.history[i], AgyHistoryItem::Tool { name: n, input: inp } if n == name && inp == input)
            };
            (guard.tool_cursor..guard.history.len())
                .find(|&i| matches(i))
                .or_else(|| (0..guard.tool_cursor).find(|&i| matches(i)))
        };
        if consume_agy_unresumed(&mut self.unresumed, |it| matches!(it, AgyWakeReplayItem::Tool { name: n, input: i } if n == name && i == input)) {
            self.replaying = true;
            if let Some(idx) = find_tool(self) {
                self.used_tools.insert(idx);
                self.tool_cursor = self.tool_cursor.max(idx + 1);
                self.max_consumed = self.max_consumed.max(idx + 1);
            }
            return true;
        }
        if let Some(idx) = find_tool(self) {
            self.used_tools.insert(idx);
            self.tool_cursor = self.tool_cursor.max(idx + 1);
            self.max_consumed = self.max_consumed.max(idx + 1);
            self.replaying = true;
            return true;
        }
        false
    }
}

fn flush(st: &mut State, em: &Emitter) {
    let Some((seg, id, mut text)) = st.seg.take() else {
        return;
    };
    if st.loading && seg == Seg::User && !text.trim().is_empty() {
        st.replay_users += 1;
    }
    if text.trim().is_empty() || st.muted() {
        return;
    }
    if st.is_antigravity && !st.loading && seg != Seg::User {
        if let Some(guard) = st.agy_replay.as_mut() {
            match guard.consume_text(seg, &text) {
                AgyReplayAction::Drop => return,
                AgyReplayAction::StripPrefix(rest) => {
                    st.agy_replay = None;
                    text = rest;
                }
                AgyReplayAction::Pass => {
                    st.agy_replay = None;
                }
            }
        }
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
    // Antigravity echoes the prompt as user_message_chunk in a live turn too; the hub already recorded it.
    // Flush any preceding segment first so a replayed reply before the prompt echo can't concatenate with the new turn.
    if seg == Seg::User && !st.loading {
        flush(st, em);
        if st.is_antigravity {
            if let Some(guard) = st.agy_replay.as_mut() {
                if guard.on_user_chunk(text) {
                    st.agy_replay = None;
                }
            }
        }
        return;
    }
    if seg != Seg::User && !st.loading && !st.subagents.is_empty() {
        return;
    }
    // The emulated /compact's summary is handed to the next session, not shown as a reply
    if !st.loading {
        if let Some(c) = st.cursor_compact.as_mut().filter(|c| !c.replacing) {
            if seg == Seg::Agent {
                c.summary.push_str(text);
            }
            return;
        }
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
            if st.is_antigravity {
                if let Some(guard) = st.agy_replay.as_mut() {
                    guard.clear_user_buf();
                    if guard.might_match_prefix(seg, buf) {
                        return;
                    }
                    // Diverged from pre-turn history: disarm guard and emit the buffered text as the first delta.
                    st.agy_replay = None;
                    match seg {
                        Seg::Agent => em.event(EventBody::AssistantDelta { id: id.clone(), delta: buf.clone() }),
                        Seg::Thought => em.event(EventBody::ThinkingDelta { id: id.clone(), delta: buf.clone() }),
                        Seg::User => {}
                    }
                    return;
                }
            }
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
            if st.cursor_compact.is_some() && matches!(kind, Pending::Prompt | Pending::Session) {
                let msg = if kind == Pending::Session { format!("压缩上下文时创建新会话失败: {msg}") } else { msg };
                cursor_compact_fail(st, em, Some(msg), "error");
                return Ok(out);
            }
            return match kind {
                // Antigravity's session/resume can fail when the session needs session/load (or on older agy builds).
                Pending::Resume if !is_auth_error(&msg) => {
                    if let Some(sid) = spec.resume.as_deref() {
                        out.push(load_session(st, spec, sid));
                        Ok(out)
                    } else {
                        Err(login_hint(spec, &msg))
                    }
                }
                // If the process went away after the session was created but before anything was said, some tools (Cursor) never saved the session and load reports not found.
                // Also if resuming across switched accounts fails for a non-auth reason (e.g. team/model mismatch), start a fresh session with the handoff digest.
                Pending::Load if is_missing_session(&msg) || (spec.account_handoff.is_some() && !is_auth_error(&msg)) => {
                    st.loading = false;
                    st.resumed = false;
                    // Fresh ACP session has not seen the memory snapshot yet
                    if st.memory_enabled {
                        st.memory_notice = em
                            .memory_prompt_snapshot()
                            .map(|p| format!("<system-reminder>\n{p}\n</system-reminder>"))
                            .or_else(|| super::acp_memory_reminder(spec));
                    }
                    if spec.account_handoff.is_some() {
                        st.account_handoff = spec.account_handoff.clone();
                        em.resume_fallback();
                    }
                    out.push(new_session(st, spec));
                    Ok(out)
                }
                Pending::Session | Pending::Load | Pending::Resume if spec.tool != ToolKind::Cursor && !st.auth_tried && st.auth_method.is_some() => {
                    st.auth_tried = true;
                    let mid = st.auth_method.clone().unwrap_or_default();
                    out.push(st.req(Pending::Auth, "authenticate", json!({"methodId": mid})));
                    Ok(out)
                }
                Pending::Auth | Pending::Session | Pending::Load | Pending::Resume | Pending::Init => Err(login_hint(spec, &msg)),
                Pending::Prompt => {
                    st.prompting = false;
                    st.end_subagents();
                    flush(st, em);
                    st.agy_replay = None;
                    st.agy_replay_hidden.clear();
                    if let Some(m) = st.agy_compact.take() {
                        st.compact_manual = false;
                        let duration_ms = u64::try_from((now_ms() - m.started).max(0)).ok();
                        em.event(EventBody::Compact {
                            phase: "failed".into(),
                            trigger: Some("manual".into()),
                            pre_tokens: m.pre_tokens,
                            post_tokens: None,
                            duration_ms,
                        });
                    }
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
                    st.thought_current = None;
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
                Pending::AgyResume | Pending::Other => Ok(out),
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
                st.can_resume = r["agentCapabilities"]["sessionCapabilities"]["resume"].is_object();
                // If an explicit auth_method (e.g. "gemini-api-key") is selected for the session,
                // authenticate upfront before session/new so it wins over host cached OAuth.
                if let Some(explicit) = &spec.auth_method {
                    st.auth_method = Some(explicit.clone());
                    st.auth_tried = true;
                    out.push(st.req(Pending::Auth, "authenticate", json!({"methodId": explicit})));
                } else {
                    // Antigravity's first method is a browser sign-in on the host; leave it to `agy acp --login`
                    st.auth_method = r["authMethods"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(|m| m["id"].as_str())
                        .filter(|_| spec.tool != ToolKind::Antigravity)
                        .map(str::to_string);
                    out.push(new_or_load(st, spec));
                }
            }
            Pending::Auth => out.push(new_or_load(st, spec)),
            Pending::Session | Pending::Load | Pending::Resume => {
                // The fresh session that replaces the compacted one
                let compacted = match &st.cursor_compact {
                    Some(c) if c.replacing && kind == Pending::Session => st.cursor_compact.take(),
                    _ => None,
                };
                if let Some(c) = &compacted {
                    // Sent after the model switch below, like the parameters restored on a resume
                    st.config_queue = c.saved.clone();
                    st.wanted = c.saved.clone();
                }
                if let Some(sid) = r["sessionId"].as_str() {
                    st.session_id = Some(sid.to_string());
                    em.external_id(sid);
                } else if let (true, Some(sid)) = (st.resumed, &spec.resume) {
                    st.session_id = Some(sid.clone());
                }
                flush(st, em);
                st.loading = false;
                if st.is_antigravity {
                    if let Some(sid) = st.session_id.as_deref() {
                        st.agy_wake_from = count_agy_transcript_steps(&agy_brain_dir(), sid);
                    }
                    if matches!(kind, Pending::Load | Pending::Resume) {
                        // Guard the first prompt after resume against any pre-turn history agy may still replay.
                        st.agy_last_streamed_steps = 0;
                    }
                }
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
                    if agy {
                        st.agy_models = models.clone();
                        st.agy_model = current.clone();
                        let active = want.as_deref().filter(|w| listed(w)).or(current.as_deref());
                        if let Some(m) = active.and_then(|id| st.agy_models.iter().find(|o| o.id == id)) {
                            if st.effort.as_deref().is_some_and(|e| !m.efforts.iter().any(|l| l == e)) {
                                st.effort = None;
                            }
                        }
                    }
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
                if let Some(c) = compacted {
                    cursor_compact_done(st, spec, em, c);
                } else if st.queue.is_empty() {
                    em.status(SessionStatus::Idle, None);
                }
            }
            Pending::Prompt if st.cursor_compact.is_some() => {
                out.extend(cursor_compact_summarized(st, spec, em, r));
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
                let wake_already_in_log = if st.is_antigravity {
                    st.agy_replay = None;
                    st.agy_replay_hidden.clear();
                    settle_agy_prompt_bg(st, em);
                    let steps = st.agy_parent_transcript.steps.clone();
                    settle_agy_compactions(st, spec, em, &steps);
                    st.agy_wake_from = agy_prompt_boundary(&steps, st.agy_pre_turn_steps, &st.last_reply);
                    st.agy_last_streamed_steps = st.agy_wake_from;
                    steps.iter().skip(st.agy_wake_from).any(is_agy_wakeup)
                } else {
                    false
                };
                let has_pending_bg = st.agy_bg_tasks.iter().any(|t| !t.done && !t.is_daemon);
                let should_watch = st.is_antigravity
                    && (!st.agy_carry.is_empty()
                        || (meta_err.is_none()
                            && limit_left.is_none()
                            && stop_reason.as_deref() != Some("cancelled")
                            && (!st.agy_pending_calls.is_empty()
                                || !st.agy_open_bg_calls.is_empty()
                                || has_pending_bg
                                || wake_already_in_log
                                || st.session_id.as_deref().is_some_and(|sid| {
                                    has_pending_agy_async_turn(&agy_brain_dir(), sid, st.agy_pre_turn_steps)
                                }))));
                if should_watch {
                    if let Some(sid) = st.session_id.clone() {
                        let mut watch = AgyWatch::new(
                            sid,
                            agy_brain_dir(),
                            st.agy_pre_turn_steps,
                            st.agy_wake_from,
                            stop_reason,
                            std::mem::take(&mut st.agy_pending_calls),
                            std::mem::take(&mut st.agy_send_baselines),
                            st.perm != "bypass",
                        )
                        .with_open_bg_calls(std::mem::take(&mut st.agy_open_bg_calls));
                        watch.adopt(std::mem::take(&mut st.agy_carry));
                        watch.adopt_bg(std::mem::take(&mut st.agy_bg_tasks), std::mem::take(&mut st.agy_seen_system_steps));
                        // The turn ends now; subagents and background tasks go on in the background
                        watch.detached = true;
                        // One pass right away, so tasks that already finished are picked up without waiting for a tick
                        let step = step_agy_watch(&mut watch, spec, em, &mut st.next_id, &mut st.pending);
                        mark_watch_settled(st, Some(spec), em, &mut watch);
                        st.prompting = false;
                        st.end_subagents();
                        // A wakeup already seen in this pass reopened the turn: that one ends it
                        let reopened = watch.reopened;
                        if !reopened {
                            em.event(EventBody::TurnEnd { stop_reason: watch.stop_reason.clone(), model: None });
                        }
                        match step {
                            AgyStep::Done(req) => {
                                remember_agy_wake_progress(st, &watch);
                                st.agy_carry.unresumed_items.append(&mut watch.unresumed_items);
                                out.extend(req);
                                if reopened {
                                    em.event(agy_wake_end(&watch));
                                }
                            }
                            AgyStep::CycleEnd => {
                                remember_agy_wake_progress(st, &watch);
                                if reopened {
                                    watch.reopened = false;
                                    em.event(agy_wake_end(&watch));
                                }
                                watch.wake = false;
                                watch.wake_failed = false;
                                st.agy_watch = Some(watch);
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
                for mut t in st.agy_bg_tasks.drain(..) {
                    if !t.done {
                        if let Some(id) = t.tc_id.take() {
                            if let Some(tool) = st.tools.get_mut(&id) {
                                tool.done = true;
                            }
                            em.event(EventBody::ToolResult {
                                id,
                                output: String::new(),
                                is_error: false,
                            });
                        }
                    }
                }
                st.agy_open_bg_calls.clear();
                st.agy_seen_system_steps.clear();
                if st.is_antigravity {
                    em.background_tasks(current_agy_tasks(st));
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
                    if st.is_antigravity {
                        st.agy_model = Some(m.clone());
                        read_thought_level(st, r);
                        if st.effort.as_deref().is_some_and(|e| !agy_model_supports_effort(st, e)) {
                            st.effort = None;
                        }
                    }
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
            Pending::AgyResume => {
                st.agy_carry.unresumed_items.clear();
                st.agy_last_streamed_steps = st.agy_last_streamed_steps.max(st.agy_wake_from);
            }
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
            // A yi_browser, yi_memory or yi_session call: the gateway enforces plan mode, write policy, family scope and secret checks
            if !question {
                if let Some(name) = gateway_tool(&known.name, &title, &tc["rawInput"]) {
                    if em.browser_tool_allowed(&name) || em.memory_tool_allowed(&name) || em.session_tool_allowed(&name) {
                        if let Some(o) = options.iter().find(|o| o.kind == "allow_once").or_else(|| options.iter().find(|o| o.kind.starts_with("allow"))) {
                            out.push(json!({"jsonrpc":"2.0","id":v["id"],
                                "result":{"outcome":{"outcome":"selected","optionId":o.id}}}));
                            return Ok(out);
                        }
                    }
                }
            }
            st.perms.insert(rid.clone(), v["id"].clone());
            agy_detached_permission(st, params);
            if st.agy_watch.is_some() {
                em.watch_active(true);
            }
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
        } else if method == "elicitation/create" {
            // ACP form elicitation (Antigravity's `ask_question` and MCP form elicitations):
            // {sessionId, toolCallId?, mode: "form", message, requestedSchema: {type: "object", properties: {...}}}
            flush(st, em);
            let tc_id = params["toolCallId"].as_str().unwrap_or("");
            let fallback_input = (!tc_id.is_empty()).then(|| st.tools.get(tc_id).map(|t| &t.input)).flatten();
            if let Some((questions, fields)) = parse_elicitation(params, fallback_input) {
                st.elicit_seq += 1;
                let rid = if tc_id.is_empty() {
                    format!("elicit:{}", st.elicit_seq)
                } else {
                    format!("elicit:{tc_id}:{}", st.elicit_seq)
                };
                let input = fallback_input
                    .filter(|inp| !inp.is_null() && *inp != &json!({}))
                    .cloned()
                    .unwrap_or_else(|| params.clone());
                st.elicits.insert(rid.clone(), PendingElicit { rpc_id: v["id"].clone(), fields });
                agy_detached_permission(st, params);
                if st.agy_watch.is_some() {
                    em.watch_active(true);
                }
                em.event(EventBody::PermissionRequest {
                    request_id: rid,
                    tool_name: "ask_question".into(),
                    title: elicit_title(spec.tool, params["message"].as_str()),
                    input,
                    options: super::question_options(),
                    questions,
                });
            } else {
                out.push(json!({"jsonrpc":"2.0","id":v["id"],"result":{"action":"decline"}}));
            }
        } else {
            out.push(json!({"jsonrpc":"2.0","id":v["id"],
                "error":{"code":-32601,"message":"not supported by yi-console"}}));
        }
        return Ok(out);
    }

    // Notifications
    match method {
        "session/update" => {
            if agy_detached_update(st, params, agy_brain_dir) {
                if st.agy_watch.is_some() {
                    em.watch_active(true);
                }
            } else {
                update(st, em, &params["update"]);
            }
        }
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

fn elicit_title(tool: ToolKind, msg: Option<&str>) -> String {
    let m = msg.map(str::trim).unwrap_or("");
    let generic = m.is_empty()
        || m == "The agent has a question."
        || (m.starts_with("The agent has ") && m.ends_with(" questions."));
    if generic {
        let name = match tool {
            ToolKind::Antigravity => "Antigravity",
            ToolKind::Cursor => "Cursor",
            ToolKind::Kiro => "Kiro",
            ToolKind::Qoder => "Qoder",
            ToolKind::Codebuddy => "CodeBuddy",
            _ => tool.as_str(),
        };
        format!("{name} 有问题想问你")
    } else {
        m.to_string()
    }
}

fn elicit_options(prop: &Value) -> Vec<QuestionOption> {
    let src = if prop["type"] == "array" { &prop["items"] } else { prop };
    if let Some(arr) = src["oneOf"].as_array().or_else(|| src["anyOf"].as_array()) {
        return arr
            .iter()
            .filter_map(|o| {
                let id = match &o["const"] {
                    Value::String(s) if !s.is_empty() => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => return None,
                };
                let label = o["title"].as_str().filter(|s| !s.is_empty()).unwrap_or(&id).to_string();
                let description = o["description"].as_str().unwrap_or("").to_string();
                Some(QuestionOption { id, label, description })
            })
            .collect();
    }
    if let Some(arr) = src["enum"].as_array() {
        let names = src["enumNames"].as_array();
        return arr
            .iter()
            .enumerate()
            .filter_map(|(i, val)| {
                let id = match val {
                    Value::String(s) if !s.is_empty() => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => return None,
                };
                let label = names
                    .and_then(|n| n.get(i))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&id)
                    .to_string();
                Some(QuestionOption { id, label, description: String::new() })
            })
            .collect();
    }
    vec![]
}

/// Parse ACP `elicitation/create` (`mode: "form"`). Antigravity builds `q1`/`q1_other`, `q2`/`q2_other`, … for
/// `ask_question`; remaining JSON Schema properties are handled as generic form questions.
fn parse_elicitation(params: &Value, fallback_input: Option<&Value>) -> Option<(Vec<Question>, Vec<ElicitField>)> {
    if params["mode"].as_str().is_some_and(|m| m != "form") {
        return None;
    }
    let mut questions = vec![];
    let mut fields = vec![];
    if let Some(props) = params["requestedSchema"]["properties"].as_object() {
        let mut consumed = HashSet::new();
        // Read q1, q2, …, qN numerically so serde_json's BTreeMap key order ("q10" before "q2") preserves question order
        for k in 1usize.. {
            let sk = format!("q{k}");
            let ok = format!("q{k}_other");
            let sel = props.get(&sk);
            let oth = props.get(&ok);
            if sel.is_none() && oth.is_none() {
                break;
            }
            if let Some(s) = sel {
                consumed.insert(sk.clone());
                let has_other = oth.is_some_and(|o| o["type"].as_str().unwrap_or("string") == "string");
                if has_other {
                    consumed.insert(ok.clone());
                }
                let multi = s["type"] == "array";
                let options = elicit_options(s);
                let question = s["title"]
                    .as_str()
                    .filter(|t| !t.is_empty())
                    .or_else(|| s["description"].as_str().filter(|t| !t.is_empty()))
                    .unwrap_or(&sk)
                    .to_string();
                let secret = s["format"] == "password" || s["writeOnly"] == json!(true);
                let (select_key, other_key, allow_other) = if options.is_empty() && !has_other {
                    (None, Some(sk.clone()), true)
                } else {
                    (Some(sk.clone()), has_other.then(|| ok.clone()), has_other)
                };
                questions.push(Question {
                    id: sk.clone(),
                    header: String::new(),
                    question,
                    options,
                    multi_select: multi,
                    allow_other,
                    secret,
                });
                fields.push(ElicitField::AskQuestion { question_id: sk, select_key, other_key, multi });
            } else if let Some(o) = oth {
                consumed.insert(ok.clone());
                let question = o["title"]
                    .as_str()
                    .filter(|t| !t.is_empty() && *t != "Other")
                    .or_else(|| {
                        o["description"]
                            .as_str()
                            .and_then(|d| d.strip_prefix("Optional free-form answer to: "))
                            .filter(|t| !t.is_empty())
                    })
                    .or_else(|| o["description"].as_str().filter(|t| !t.is_empty()))
                    .unwrap_or(&ok)
                    .to_string();
                let secret = o["format"] == "password" || o["writeOnly"] == json!(true);
                questions.push(Question {
                    id: sk.clone(),
                    header: String::new(),
                    question,
                    options: vec![],
                    multi_select: false,
                    allow_other: true,
                    secret,
                });
                fields.push(ElicitField::AskQuestion { question_id: sk, select_key: None, other_key: Some(ok), multi: false });
            }
        }
        for (key, prop) in props {
            if consumed.contains(key) {
                continue;
            }
            let ptype = prop["type"].as_str().unwrap_or("string");
            let title = prop["title"].as_str().map(str::trim).unwrap_or("");
            let desc = prop["description"].as_str().map(str::trim).unwrap_or("");
            let (header, question) = if !title.is_empty() && !desc.is_empty() && title != desc && title.chars().count() <= 32 {
                (title.to_string(), desc.to_string())
            } else {
                (
                    String::new(),
                    if !title.is_empty() {
                        title
                    } else if !desc.is_empty() {
                        desc
                    } else {
                        key.as_str()
                    }
                    .to_string(),
                )
            };
            let secret = prop["format"] == "password" || prop["writeOnly"] == json!(true);
            let opts = elicit_options(prop);
            let (options, multi_select, allow_other, kind) = if ptype == "boolean" {
                (
                    vec![
                        QuestionOption { id: "true".into(), label: "是".into(), description: String::new() },
                        QuestionOption { id: "false".into(), label: "否".into(), description: String::new() },
                    ],
                    false,
                    false,
                    ElicitPropKind::Bool,
                )
            } else if !opts.is_empty() {
                let multi = ptype == "array";
                (opts, multi, false, if multi { ElicitPropKind::MultiArray } else { ElicitPropKind::String })
            } else {
                let kind = match ptype {
                    "integer" => ElicitPropKind::Integer,
                    "number" => ElicitPropKind::Number,
                    _ => ElicitPropKind::String,
                };
                (vec![], false, true, kind)
            };
            questions.push(Question {
                id: key.clone(),
                header,
                question,
                options,
                multi_select,
                allow_other,
                secret,
            });
            fields.push(ElicitField::Property { question_id: key.clone(), key: key.clone(), kind });
        }
    }
    if questions.is_empty() {
        if let Some(arr) = fallback_input.and_then(|v| v["questions"].as_array()) {
            for (idx, q) in arr.iter().enumerate() {
                let k = idx + 1;
                let sk = format!("q{k}");
                let ok = format!("q{k}_other");
                let question = q["question"].as_str().unwrap_or("").trim().to_string();
                if question.is_empty() {
                    continue;
                }
                let multi = q["is_multi_select"].as_bool().or_else(|| q["multiSelect"].as_bool()).unwrap_or(false);
                let options: Vec<QuestionOption> = q["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .filter_map(|(oi, o)| {
                        if let Some(s) = o.as_str().filter(|s| !s.is_empty()) {
                            Some(QuestionOption { id: (oi + 1).to_string(), label: s.to_string(), description: String::new() })
                        } else {
                            let label = o["label"].as_str().or_else(|| o["text"].as_str())?.to_string();
                            let id = o["id"].as_str().map(str::to_string).unwrap_or_else(|| (oi + 1).to_string());
                            let description = o["description"].as_str().unwrap_or("").to_string();
                            Some(QuestionOption { id, label, description })
                        }
                    })
                    .collect();
                let select_key = (!options.is_empty()).then(|| sk.clone());
                questions.push(Question {
                    id: sk.clone(),
                    header: q["header"].as_str().unwrap_or("").to_string(),
                    question,
                    options,
                    multi_select: multi,
                    allow_other: true,
                    secret: false,
                });
                fields.push(ElicitField::AskQuestion { question_id: sk, select_key, other_key: Some(ok), multi });
            }
        }
    }
    (!questions.is_empty()).then_some((questions, fields))
}

fn elicit_response(el: &PendingElicit, option_id: &str, answers: &[QuestionAnswer]) -> Value {
    let result = match option_id {
        QUESTION_ANSWER => {
            let mut content = serde_json::Map::new();
            for field in &el.fields {
                match field {
                    ElicitField::AskQuestion { question_id, select_key, other_key, multi } => {
                        let Some(ans) = answers.iter().find(|a| a.question_id == *question_id) else { continue };
                        if let Some(sk) = select_key {
                            if !ans.selected.is_empty() {
                                let v = if *multi { json!(ans.selected) } else { json!(ans.selected[0]) };
                                content.insert(sk.clone(), v);
                            }
                        }
                        let text = ans.text.trim();
                        if !text.is_empty() {
                            if let Some(ok) = other_key.as_ref().or(select_key.as_ref()) {
                                content.insert(ok.clone(), json!(text));
                            }
                        }
                    }
                    ElicitField::Property { question_id, key, kind } => {
                        let Some(ans) = answers.iter().find(|a| a.question_id == *question_id) else { continue };
                        match kind {
                            ElicitPropKind::MultiArray => {
                                if !ans.selected.is_empty() {
                                    content.insert(key.clone(), json!(ans.selected));
                                }
                            }
                            ElicitPropKind::Bool => {
                                let raw = ans.selected.first().map(String::as_str).unwrap_or_else(|| ans.text.trim());
                                if let Ok(b) = raw.parse::<bool>() {
                                    content.insert(key.clone(), json!(b));
                                }
                            }
                            ElicitPropKind::Integer => {
                                let raw = ans.selected.first().map(String::as_str).unwrap_or_else(|| ans.text.trim());
                                if let Ok(n) = raw.parse::<i64>() {
                                    content.insert(key.clone(), json!(n));
                                } else if !raw.is_empty() {
                                    content.insert(key.clone(), json!(raw));
                                }
                            }
                            ElicitPropKind::Number => {
                                let raw = ans.selected.first().map(String::as_str).unwrap_or_else(|| ans.text.trim());
                                if let Ok(n) = raw.parse::<f64>() {
                                    content.insert(key.clone(), json!(n));
                                } else if !raw.is_empty() {
                                    content.insert(key.clone(), json!(raw));
                                }
                            }
                            ElicitPropKind::String => {
                                let raw = ans
                                    .selected
                                    .first()
                                    .map(String::as_str)
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or_else(|| ans.text.trim());
                                if !raw.is_empty() {
                                    content.insert(key.clone(), json!(raw));
                                }
                            }
                        }
                    }
                }
            }
            json!({"action":"accept","content":content})
        }
        "cancelled" => json!({"action":"cancel"}),
        _ => json!({"action":"decline"}),
    };
    json!({"jsonrpc":"2.0","id":el.rpc_id,"result":result})
}

/// The `mcp__<server>__<tool>` name of a permission request for a built-in gateway tool (`yi_browser` or `yi_memory`).
/// Exact forms only, never a substring of free input (a shell command mentioning the name must not be approved):
/// Qoder names the call `mcp__<server>__<tool>` and titles it `<tool> (<server>)`; CodeBuddy wraps it in
/// `DeferExecuteTool` with `toolName` in the input
fn gateway_tool(name: &str, title: &str, input: &Value) -> Option<String> {
    let check_server = |server: &str, is_known: &dyn Fn(&str) -> bool| -> Option<String> {
        let prefix = format!("mcp__{server}__");
        let known = |n: &str| n.strip_prefix(&prefix).filter(|t| is_known(t)).map(|_| n.to_string());
        if let Some(n) = known(name) {
            return Some(n);
        }
        if name == "DeferExecuteTool" {
            if let Some(n) = input["toolName"].as_str().and_then(known) {
                return Some(n);
            }
        }
        let suffix = format!(" ({server})");
        title.strip_suffix(&suffix).filter(|t| is_known(t)).map(|t| format!("{prefix}{t}"))
    };
    check_server(crate::mcp::BROWSER_SERVER, &|t| yi_browser::tool_names().any(|x| x == t))
        .or_else(|| check_server(crate::mcp::MEMORY_SERVER, &|t| crate::memory::tool_names().any(|x| x == t)))
        .or_else(|| check_server(crate::mcp::SESSION_SERVER, &|t| crate::session_mcp::tool_names().any(|x| x == t)))
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
    if ok && st.memory_enabled {
        // Compaction may have summarized away the initial <yi-memory> block: re-arm a fresh snapshot
        if let Some(p) = em.memory_prompt_snapshot() {
            st.memory_notice = Some(format!("<system-reminder>\n{p}\n</system-reminder>"));
        }
    }
    em.event(EventBody::Compact {
        phase: if ok { "done" } else { "failed" }.into(),
        trigger: ok.then(|| if std::mem::take(&mut st.compact_manual) { "manual" } else { "auto" }.into()),
        pre_tokens: None,
        post_tokens: None,
        duration_ms: ok.then(|| (now_ms() - t0).max(0) as u64),
    });
}

fn new_or_load(st: &mut State, spec: &StartSpec) -> Value {
    match &spec.resume {
        Some(sid) if st.is_antigravity && !spec.replay && st.can_resume => resume_session(st, spec, sid),
        Some(sid) => load_session(st, spec, sid),
        None => new_session(st, spec),
    }
}

fn resume_session(st: &mut State, spec: &StartSpec, sid: &str) -> Value {
    let cwd = spec.cwd.display().to_string();
    st.loading = true;
    st.record_replay = false;
    st.replay_skip = 0;
    st.replay_users = 0;
    st.resumed = true;
    st.req(
        Pending::Resume,
        "session/resume",
        json!({"sessionId":sid,"cwd":cwd,"mcpServers":spec.mcp_servers}),
    )
}

fn load_session(st: &mut State, spec: &StartSpec, sid: &str) -> Value {
    let cwd = spec.cwd.display().to_string();
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

fn new_session(st: &mut State, spec: &StartSpec) -> Value {
    let cwd = spec.cwd.display().to_string();
    st.req(Pending::Session, "session/new", json!({"cwd":cwd,"mcpServers":spec.mcp_servers}))
}

/// Antigravity manual `/compact` turn in progress
struct AgyCompact {
    started: i64,
    pre_tokens: Option<u64>,
}

fn rearm_memory_notice(st: &mut State, spec: Option<&StartSpec>, em: &Emitter) {
    if st.memory_enabled {
        // Compaction may have summarized away the initial <yi-memory> block: re-arm a fresh snapshot
        st.memory_notice = em
            .memory_prompt_snapshot()
            .map(|p| format!("<system-reminder>\n{p}\n</system-reminder>"))
            .or_else(|| spec.and_then(super::acp_memory_reminder));
    }
}

fn is_agy_checkpoint(step: &Value) -> bool {
    step["type"].as_str() == Some("CHECKPOINT") && step["status"].as_str() == Some("DONE")
}

fn agy_step_input_tokens(step: &Value) -> Option<u64> {
    let total = step["input_tokens"].as_u64().unwrap_or(0) + step["cache_read_tokens"].as_u64().unwrap_or(0);
    (total > 0).then_some(total)
}

fn agy_step_total_tokens(step: &Value) -> Option<u64> {
    let total = step["input_tokens"].as_u64().unwrap_or(0)
        + step["cache_read_tokens"].as_u64().unwrap_or(0)
        + step["output_tokens"].as_u64().unwrap_or(0);
    (total > 0).then_some(total)
}

fn agy_last_step_tokens(steps: &[Value]) -> Option<u64> {
    steps
        .iter()
        .rev()
        .filter(|s| s["type"].as_str() == Some("PLANNER_RESPONSE"))
        .find_map(agy_step_total_tokens)
}

fn agy_post_checkpoint_tokens(after: &[Value]) -> Option<u64> {
    after
        .iter()
        .take_while(|s| !is_agy_checkpoint(s))
        .filter(|s| s["type"].as_str() == Some("PLANNER_RESPONSE"))
        .find_map(agy_step_input_tokens)
}

/// Inspect Antigravity transcript steps appended during this prompt for `CHECKPOINT` compaction steps
/// (both manual `/compact` and automatic compaction) and settle any in-progress manual compaction
fn settle_agy_compactions(st: &mut State, spec: &StartSpec, em: &Emitter, steps: &[Value]) {
    let manual_started = st.agy_compact.take();
    let is_manual_cmd = std::mem::take(&mut st.compact_manual);
    let checkpoints: Vec<usize> = steps
        .iter()
        .enumerate()
        .skip(st.agy_pre_turn_steps)
        .filter(|(idx, step)| is_agy_checkpoint(step) && !st.agy_seen_checkpoints.contains(idx))
        .map(|(idx, _)| idx)
        .collect();
    if !checkpoints.is_empty() {
        let mut last_post = None;
        for (pos, idx) in checkpoints.into_iter().enumerate() {
            st.agy_seen_checkpoints.insert(idx);
            let step = &steps[idx];
            let manual = pos == 0 && (is_manual_cmd || manual_started.is_some());
            let pre_tokens = agy_last_step_tokens(&steps[..idx])
                .or_else(|| (pos == 0).then(|| manual_started.as_ref().and_then(|m| m.pre_tokens)).flatten())
                .or_else(|| agy_step_input_tokens(step));
            let post_tokens = agy_post_checkpoint_tokens(&steps[idx + 1..]);
            last_post = post_tokens;
            let duration_ms = (pos == 0)
                .then(|| manual_started.as_ref().and_then(|m| u64::try_from((now_ms() - m.started).max(0)).ok()))
                .flatten();
            em.event(EventBody::Compact {
                phase: "done".into(),
                trigger: Some(if manual { "manual" } else { "auto" }.into()),
                pre_tokens,
                post_tokens,
                duration_ms,
            });
        }
        rearm_memory_notice(st, Some(spec), em);
        // Bare `/compact` emits only the summarizer call's pre-compact `usage_update` with no follow-up turn.
        // Clear the gauge so the UI and Bot auto-compact don't see stale pre-compaction context usage.
        if last_post.is_none() {
            st.agy_context_tokens = None;
            em.context(0.0);
        }
    } else if let Some(m) = manual_started {
        let duration_ms = u64::try_from((now_ms() - m.started).max(0)).ok();
        em.event(EventBody::Compact {
            phase: "failed".into(),
            trigger: Some("manual".into()),
            pre_tokens: m.pre_tokens,
            post_tokens: None,
            duration_ms,
        });
    }
}

/// Cursor's emulated /compact in progress
#[derive(Default)]
struct CursorCompact {
    started: i64,
    /// The summary the tool is writing (kept out of the chat)
    summary: String,
    /// The summary is done and `session/new` is out
    replacing: bool,
    /// The model's parameters, set again on the fresh session
    saved: Vec<(String, String)>,
}

const CURSOR_COMPACT_PROMPT: &str = "The user ran /compact to free up context. Write a summary of this whole conversation that will \
replace it: a fresh session will continue from your summary alone. Do not call any tools and do not continue the task. Cover the \
user's goals and explicit requests, decisions and constraints, files and code touched (paths and the key changes), commands run and \
their results, errors and how they were resolved, the current state, and pending next steps. Keep exact names, paths and values. \
Reply with the summary only.";

const CURSOR_COMPACT_INTRO: &str = "The earlier conversation in this session was compacted. This summary of it replaces the \
history; continue from it without repeating completed work.";

/// `/compact` or `/compact <instructions>` as typed by the user
pub fn is_compact_command(text: &str) -> bool {
    let t = text.trim();
    t == "/compact" || t.strip_prefix("/compact").is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

/// A /compact message, possibly behind context the hub put first (`<yi-…>…</yi-…>` blocks):
/// returns that context and the user's extra instructions
fn split_compact(text: &str) -> Option<(String, String)> {
    let mut t = text.trim_start();
    let mut prior = vec![];
    while t.starts_with("<yi-") {
        let name = &t[1..t.find('>')?];
        let close = format!("</{name}>");
        let end = t.find(&close)? + close.len();
        prior.push(t[..end].to_string());
        t = t[end..].trim_start();
    }
    is_compact_command(t).then(|| (prior.join("\n\n"), t.trim()["/compact".len()..].trim().to_string()))
}

/// Cursor's ACP has no compaction (its terminal UI's /summarize isn't exposed), and it treats /compact as a plain prompt.
/// So the tool writes a summary (hidden), a fresh ACP session replaces the old one, and the hub puts the summary before
/// the next message. Messages sent meanwhile wait in the queue (`compacting`)
fn cursor_compact_start(st: &mut State, em: &Emitter, prior: &str, extra: String) -> Value {
    let now = now_ms();
    st.compacting = Some(now);
    st.cursor_compact = Some(CursorCompact { started: now, ..Default::default() });
    em.event(EventBody::Compact { phase: "start".into(), trigger: Some("manual".into()), pre_tokens: None, post_tokens: None, duration_ms: None });
    let mut text = CURSOR_COMPACT_PROMPT.to_string();
    if !prior.is_empty() {
        text.push_str("\n\nContext carried over from before this session, to include in the summary:\n");
        text.push_str(prior);
    }
    if !extra.is_empty() {
        text.push_str("\n\nAdditional instructions from the user for the summary: ");
        text.push_str(&extra);
    }
    let sid = st.session_id.clone().unwrap_or_default();
    st.req(Pending::Prompt, "session/prompt", json!({"sessionId":sid,"prompt":[{"type":"text","text":text}]}))
}

/// The summary turn ended: open the fresh session, or give up and keep the old one
fn cursor_compact_summarized(st: &mut State, spec: &StartSpec, em: &Emitter, r: &Value) -> Vec<Value> {
    let stop = r["stopReason"].as_str().unwrap_or("end_turn").to_string();
    if let Some(msg) = meta_error(r) {
        cursor_compact_fail(st, em, Some(login_hint(spec, &msg)), "error");
        return vec![];
    }
    if stop == "cancelled" {
        cursor_compact_fail(st, em, None, "cancelled");
        return vec![];
    }
    let saved: Vec<(String, String)> = st.config_options.iter()
        .filter(|o| !o.current_value.is_empty())
        .map(|o| (o.id.clone(), o.current_value.clone()))
        .collect();
    let Some(c) = st.cursor_compact.as_mut() else { return vec![] };
    c.summary = c.summary.trim().to_string();
    if c.summary.is_empty() {
        cursor_compact_fail(st, em, Some("压缩上下文失败：Cursor 没有返回摘要，原会话保持不变".into()), &stop);
        return vec![];
    }
    c.replacing = true;
    c.saved = saved;
    // Stay on the model in use, not the one the session was launched with
    if st.parameterized {
        st.want_model = st.config_model.clone();
    }
    st.spec_params_stale = true;
    st.resumed = false;
    vec![new_session(st, spec)]
}

/// The fresh session is up: the summary waits in the hub for the next message
fn cursor_compact_done(st: &mut State, spec: &StartSpec, em: &Emitter, c: CursorCompact) {
    st.compacting = None;
    st.prompting = false;
    st.end_subagents();
    // A new tool session hasn't seen the appended system prompt, memory index, or the computer use notice
    st.system_notice = super::acp_system_reminder(spec);
    if st.memory_enabled {
        st.memory_notice = em
            .memory_prompt_snapshot()
            .map(|p| format!("<system-reminder>\n{p}\n</system-reminder>"))
            .or_else(|| super::acp_memory_reminder(spec));
    }
    st.desktop_notice = spec.mcp_servers.iter().any(|server| server["name"] == crate::mcp::DESKTOP_SERVER);
    st.browser_notice = spec.mcp_servers.iter().any(|server| server["name"] == crate::mcp::BROWSER_SERVER);
    em.pending_context(format!("<yi-compact-summary>\n{CURSOR_COMPACT_INTRO}\n\n{}\n</yi-compact-summary>", c.summary));
    em.context(0.0);
    let duration = u64::try_from(now_ms() - c.started).ok();
    em.event(EventBody::Compact { phase: "done".into(), trigger: Some("manual".into()), pre_tokens: None, post_tokens: None, duration_ms: duration });
    em.event(EventBody::TurnEnd { stop_reason: Some("end_turn".into()), model: None });
}

fn cursor_compact_fail(st: &mut State, em: &Emitter, msg: Option<String>, stop: &str) {
    let started = st.cursor_compact.take().map(|c| c.started).unwrap_or_else(now_ms);
    st.compacting = None;
    st.prompting = false;
    st.want_model = None;
    st.end_subagents();
    if let Some(message) = msg {
        em.event(EventBody::Error { message });
    }
    let duration = u64::try_from(now_ms() - started).ok();
    em.event(EventBody::Compact { phase: "failed".into(), trigger: Some("manual".into()), pre_tokens: None, post_tokens: None, duration_ms: duration });
    em.event(EventBody::TurnEnd { stop_reason: Some(stop.to_string()), model: None });
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

fn is_auth_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("auth") || lower.contains("login") || msg.contains("401") || msg.contains("403")
}

fn login_hint(spec: &StartSpec, msg: &str) -> String {
    if is_auth_error(msg) {
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
        // Standard ACP context usage (CodeBuddy and Antigravity send it, with this turn's credit cost in cost.amount)
        "usage_update" => {
            if let (Some(used), Some(size)) = (u["used"].as_f64(), u["size"].as_f64().filter(|s| *s > 0.0)) {
                if used > 0.0 {
                    if st.is_antigravity {
                        st.agy_context_tokens = Some(used.round() as u64);
                    }
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
            let id = u["toolCallId"].as_str().unwrap_or("").to_string();
            if st.muted() {
                if !id.is_empty() {
                    st.muted_tools.insert(id);
                }
                return;
            }
            if st.hidden.contains(&id) || st.muted_tools.contains(&id) || st.agy_replay_hidden.contains(&id) {
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
            if !known && st.is_antigravity && !st.loading && u["sessionUpdate"] == "tool_call" {
                if let Some(guard) = st.agy_replay.as_mut() {
                    let agy_name = if t.name == "other" || t.name.is_empty() { t.title.as_str() } else { t.name.as_str() };
                    if guard.consume_tool(agy_name, &t.input) {
                        if !id.is_empty() {
                            st.agy_replay_hidden.insert(id);
                        }
                        return;
                    }
                    // An unmatched new tool call means the live turn has started.
                    st.agy_replay = None;
                }
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
            if (u["sessionUpdate"] == "tool_call" && !known) || (!t.done && before != after) {
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
                } else if !t.done
                    && matches!(agy_tool.to_ascii_lowercase().as_str(), "run_command" | "run_workflow" | "schedule")
                    && !st.agy_open_bg_calls.iter().any(|(x, _)| x == &id)
                {
                    st.agy_open_bg_calls.push_back((id.clone(), agy_tool.to_string()));
                }
            }
            let status = u["status"].as_str().unwrap_or("");
            if (status == "completed" || status == "failed") && !t.done {
                t.done = true;
                st.subagents.remove(&id);
                let mut output = texts.join("\n");
                if output.is_empty() && !command_text && u["rawOutput"]["output"].is_string() {
                    let raw = u["rawOutput"]["output"].as_str().unwrap_or("");
                    match antigravity_command_text(raw) {
                        Some(out) => {
                            command_text = true;
                            output = out;
                        }
                        None => output = raw.to_string(),
                    }
                }
                if output.is_empty() && !command_text && !u["rawOutput"].is_null() {
                    output = raw_output_text(&crate::computer_use::display_value(&u["rawOutput"]));
                }
                // `agy acp` reports invoke_subagent / send_message completed immediately after spawning, with empty output,
                // and background run_command / schedule / run_workflow with a "Tool is running as a background task" notice.
                // Hold the ToolResult until the background work finishes and produces its real result.
                let agy_tool = if t.name == "other" || t.name.is_empty() { t.title.as_str() } else { t.name.as_str() };
                let bg_parsed = (st.is_antigravity && !st.loading && status == "completed" && !command_text)
                    .then(|| parse_agy_bg_task(&output))
                    .flatten();
                let mut already_settled = false;
                if st.is_antigravity
                    && !st.loading
                    && bg_parsed.is_none()
                    && !command_text
                    && (output.is_empty() || output == "{}")
                    && matches!(agy_tool.to_ascii_lowercase().as_str(), "run_command" | "run_workflow" | "schedule")
                    && !st.agy_open_bg_calls.is_empty()
                {
                    poll_agy_prompt_bg(st, em);
                    already_settled = st.tools.get(&id).is_some_and(|existing| existing.done);
                }
                st.agy_open_bg_calls.retain(|(x, _)| x != &id);
                let mut defer_agy = already_settled
                    || (st.is_antigravity
                        && !st.loading
                        && status == "completed"
                        && output.is_empty()
                        && st.agy_pending_calls.contains_key(&id));
                if let Some(parsed) = bg_parsed {
                    let (kind, is_daemon, timer_cond, meta_desc) = agy_bg_meta(agy_tool, &t.input, &parsed.description);
                    if !is_daemon {
                        defer_agy = true;
                        if let Some(existing) = st.agy_bg_tasks.iter_mut().find(|b| b.id == parsed.id) {
                            if !existing.done {
                                existing.tc_id.get_or_insert_with(|| id.clone());
                            }
                        } else {
                            let desc = if !meta_desc.is_empty() { meta_desc } else { parsed.id.clone() };
                            st.agy_bg_tasks.push(AgyBgTask {
                                id: parsed.id,
                                kind,
                                description: desc,
                                started_at: now_ms(),
                                output_file: parsed.output_file,
                                tc_id: Some(id.clone()),
                                is_daemon: false,
                                timer_cond,
                                done: false,
                                announced: false,
                                result: None,
                            });
                            em.background_tasks(current_agy_tasks(st));
                        }
                    }
                } else if st.is_antigravity
                    && !st.loading
                    && (status == "failed" || status == "completed")
                    && (output.is_empty() || output == "{}")
                    && st
                        .agy_bg_tasks
                        .iter()
                        .chain(&st.agy_carry.bg_tasks)
                        .chain(st.agy_watch.as_ref().into_iter().flat_map(|w| w.bg_tasks.iter()))
                        .any(|b| !b.done && b.tc_id.as_deref() == Some(id.as_str()))
                {
                    defer_agy = true;
                }
                if !defer_agy {
                    st.agy_pending_calls.remove(&id);
                    for b in st.agy_bg_tasks.iter_mut().chain(st.agy_carry.bg_tasks.iter_mut()) {
                        if b.tc_id.as_deref() == Some(id.as_str()) {
                            b.tc_id = None;
                        }
                    }
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
            if st.qoder_config_dir.as_ref().is_some_and(|dir| em.sync_qoder_plan(dir)) {
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
            em.event(EventBody::Plan { entries, session_scoped: false });
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
/// have no sender, or use `sender=system`, and arrive inside a prompt turn without waking the parent.
fn is_agy_wakeup(step: &Value) -> bool {
    is_agy_system(step) && {
        let content = step["content"].as_str().unwrap_or("");
        content.contains("<SYSTEM_MESSAGE>")
            && extract_system_sender(content).is_some_and(|sender| sender != "system")
    }
}

fn is_agy_final_reply(step: &Value) -> bool {
    step["type"].as_str() == Some("PLANNER_RESPONSE")
        && step["status"].as_str() == Some("DONE")
        && step["tool_calls"].as_array().is_none_or(|a| a.is_empty())
}

fn agy_prompt_boundary(steps: &[Value], pre_prompt: usize, reply: &str) -> usize {
    // A wakeup can already be on disk when the prompt RPC returns. Anchor to the reply actually
    // streamed by the prompt whenever it is present, rather than skipping that wakeup.
    let reply = reply.trim();
    if !reply.is_empty() {
        if let Some((index, _)) = steps.iter().enumerate().skip(pre_prompt).find(|(_, step)| {
            is_agy_final_reply(step) && step["content"].as_str().is_some_and(|text| text.trim() == reply)
        }) {
            return index + 1;
        }
    }
    // For tool-only turns (empty reply), preserve any trailing wakeup SYSTEM_MESSAGE(s) already on disk.
    if let Some(first_trailing_wake) = steps
        .iter()
        .enumerate()
        .skip(pre_prompt)
        .rev()
        .take_while(|(_, step)| is_agy_wakeup(step))
        .last()
        .map(|(idx, _)| idx)
    {
        return first_trailing_wake;
    }
    steps.len().max(pre_prompt)
}

/// Newer agy sends parent updates outside session/prompt too. Use them to start or keep
/// the transcript watch alive. The watch owns these steps, including the final reply.
fn agy_detached_update(st: &mut State, params: &Value, brain_dir: impl FnOnce() -> PathBuf) -> bool {
    if !st.is_antigravity || st.prompting || st.loading {
        return false;
    }
    let Some(sid) = st.session_id.as_deref() else { return false };
    if params["sessionId"].as_str().is_some_and(|id| id != sid) {
        return true;
    }
    let update = &params["update"];
    match update["sessionUpdate"].as_str() {
        Some("agent_message_chunk" | "agent_thought_chunk") => {
            if content_text(&update["content"]).is_empty() {
                return true;
            }
        }
        Some("tool_call" | "tool_call_update") => {
            let Some(id) = update["toolCallId"].as_str() else { return true };
            if st.tools.get(id).is_some_and(|tool| !tool.agy_detached) {
                // The watch will publish the real result for asynchronous calls it took over.
                return st.agy_watch.as_ref().is_some_and(|watch| {
                    watch.pending_calls.contains_key(id)
                        || watch.open_bg_calls.iter().any(|(call, _)| call == id)
                        || watch.bg_tasks.iter().any(|task| task.tc_id.as_deref() == Some(id))
                });
            }
            let tool = st.tools.entry(id.to_string()).or_default();
            tool.agy_detached = true;
            // Keep the metadata for permission and elicitation requests that refer to this id.
            if let Some(title) = update["title"].as_str() {
                tool.title = title.to_string();
            }
            if tool.name.is_empty() {
                tool.name = meta_tool_name(update)
                    .or(update["name"].as_str())
                    .unwrap_or(&tool.title)
                    .to_string();
            }
            if !update["rawInput"].is_null() {
                tool.input = update["rawInput"].clone();
            }
        }
        _ => return false,
    }
    note_agy_detached_activity(st, brain_dir);
    true
}

fn agy_detached_permission(st: &mut State, params: &Value) {
    if st.is_antigravity && !st.prompting && !st.loading
        && params["sessionId"].as_str().is_none_or(|sid| st.session_id.as_deref() == Some(sid))
    {
        if let Some(watch) = note_agy_detached_activity(st, agy_brain_dir) {
            // The permission event itself puts the session in WaitingPermission.
            watch.reopened = true;
        }
    }
}

fn note_agy_detached_activity(st: &mut State, brain_dir: impl FnOnce() -> PathBuf) -> Option<&mut AgyWatch> {
    let sid = st.session_id.as_deref()?;
    let watch = st.agy_watch.get_or_insert_with(|| {
        let mut watch = AgyWatch::new(
            sid.to_string(), brain_dir(), st.agy_wake_from, st.agy_wake_from, None,
            std::mem::take(&mut st.agy_pending_calls), std::mem::take(&mut st.agy_send_baselines),
            st.perm != "bypass",
        ).with_open_bg_calls(std::mem::take(&mut st.agy_open_bg_calls));
        watch.adopt(std::mem::take(&mut st.agy_carry));
        watch.adopt_bg(std::mem::take(&mut st.agy_bg_tasks), std::mem::take(&mut st.agy_seen_system_steps));
        watch.detached = true;
        watch
    });
    watch.last_acp_activity = Some(tokio::time::Instant::now());
    Some(watch)
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

fn uri_to_path(uri: &str) -> String {
    let s = uri.trim();
    if let Some(rest) = s.strip_prefix("file:///") {
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return rest.to_string();
        }
        return format!("/{rest}");
    }
    s.strip_prefix("file://").unwrap_or(s).to_string()
}

fn arg_str(v: &Value) -> Option<String> {
    let s = v.as_str()?.trim();
    let s = s.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(s).trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn arg_bool(v: &Value) -> bool {
    v.as_bool() == Some(true) || matches!(arg_str(v).as_deref(), Some("true"))
}

#[derive(Debug, Clone)]
struct AgyBgParsed {
    id: String,
    description: String,
    output_file: Option<String>,
}

fn parse_agy_bg_task(content: &str) -> Option<AgyBgParsed> {
    if content.contains("Completed At:") {
        return None;
    }
    let id = agy_background_task(content)?.to_string();
    let description = content
        .split_once("Task Description: ")
        .map(|(_, rest)| {
            let end = [
                "\nTask logs are available at:",
                "\nLog output:",
                "\nYOU MUST TAKE ",
            ]
            .iter()
            .filter_map(|m| rest.find(m))
            .min()
            .unwrap_or(rest.len());
            crate::util::truncate(rest[..end].trim(), 200)
        })
        .unwrap_or_default();
    let output_file = content
        .split_once("Task logs are available at: ")
        .and_then(|(_, rest)| rest.lines().next())
        .map(uri_to_path)
        .filter(|s| !s.is_empty());
    Some(AgyBgParsed { id, description, output_file })
}

fn agy_bg_meta(tool_name: &str, args: &Value, parsed_desc: &str) -> (String, bool, Option<String>, String) {
    match tool_name.to_ascii_lowercase().as_str() {
        "run_command" => {
            let desc = arg_str(&args["toolSummary"])
                .or_else(|| (!parsed_desc.is_empty()).then(|| parsed_desc.to_string()))
                .or_else(|| arg_str(&args["CommandLine"]))
                .unwrap_or_default();
            ("local_bash".into(), arg_bool(&args["IsDaemon"]), None, crate::util::truncate(&desc, 200))
        }
        "run_workflow" => {
            let desc = arg_str(&args["Description"])
                .or_else(|| (!parsed_desc.is_empty()).then(|| parsed_desc.to_string()))
                .unwrap_or_default();
            ("workflow".into(), false, None, crate::util::truncate(&desc, 200))
        }
        "schedule" => {
            let is_daemon = arg_bool(&args["IsDaemon"]) || arg_str(&args["CronExpression"]).is_some();
            let timer_cond = arg_str(&args["TimerCondition"]).filter(|s| s != "never");
            let desc = if !parsed_desc.is_empty() {
                parsed_desc.to_string()
            } else {
                arg_str(&args["Prompt"]).unwrap_or_default()
            };
            ("timer".into(), is_daemon, timer_cond, crate::util::truncate(&desc, 200))
        }
        _ => {
            let kind = if parsed_desc.starts_with("Timer:") || parsed_desc.starts_with("Cron:") {
                "timer"
            } else {
                "local_bash"
            };
            (kind.into(), false, None, crate::util::truncate(parsed_desc, 200))
        }
    }
}

#[derive(Debug, Clone)]
enum AgyTaskNotice {
    Finished { id: String, output: String, is_error: bool },
    Canceled { id: String, output: String },
    Message { sender: String, output: String },
}

fn extract_task_result_body(rest: &str) -> String {
    let result = rest.split("\n\nLog: ").next().unwrap_or(rest);
    let result = result.split("\nLog: ").next().unwrap_or(result);
    let result = result.split("</SYSTEM_MESSAGE>").next().unwrap_or(result).trim();
    antigravity_command_body(result).unwrap_or_else(|| result.replace("\r\n", "\n"))
}

fn extract_system_sender(content: &str) -> Option<&str> {
    let rest = content.split_once("sender=")?.1;
    rest.split_whitespace().next().filter(|s| !s.is_empty())
}

fn agy_task_notice(content: &str) -> Option<AgyTaskNotice> {
    if let Some(rest) = content.split_once("Task id \"") {
        if let Some((task, after)) = rest.1.split_once('"') {
            if let Some((_, body)) = after.split_once("finished with result:") {
                let text = extract_task_result_body(body);
                let is_error = text.contains("exit code: ");
                return Some(AgyTaskNotice::Finished { id: task.to_string(), output: text, is_error });
            }
            if let Some((_, body)) = after.split_once("was canceled with result:") {
                let text = extract_task_result_body(body);
                return Some(AgyTaskNotice::Canceled { id: task.to_string(), output: text });
            }
        }
    }
    let sender = extract_system_sender(content)?;
    let output = extract_system_message_content(content).unwrap_or_default();
    Some(AgyTaskNotice::Message { sender: sender.to_string(), output })
}

/// A SYSTEM_MESSAGE `Task id "<id>" finished with result:\n<result>\n\nLog: …`: the task id and its result text
#[cfg(test)]
fn agy_task_finished(content: &str) -> Option<(&str, String)> {
    let rest = content.split_once("Task id \"")?.1;
    let (task, rest) = rest.split_once('"')?;
    let result = rest.split_once("finished with result:")?.1;
    Some((task, extract_task_result_body(result)))
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

fn has_pending_agy_async_turn(brain_dir: &std::path::Path, parent_sid: &str, pre_turn_steps: usize) -> bool {
    let steps = read_agy_transcript_steps(brain_dir, parent_sid);
    if pre_turn_steps >= steps.len() {
        return false;
    }
    let slice = &steps[pre_turn_steps..];
    let mut launched_sub = false;
    let mut has_bg = false;
    let mut open_calls: VecDeque<(String, Value)> = VecDeque::new();
    for s in slice {
        match s["type"].as_str().unwrap_or("") {
            "PLANNER_RESPONSE" => {
                open_calls.clear();
                for tc in s["tool_calls"].as_array().into_iter().flatten() {
                    let name = tc["name"].as_str().unwrap_or("");
                    if is_agy_subagent_tool(name, &tc["args"]) {
                        launched_sub = true;
                    }
                    open_calls.push_back((name.to_string(), tc["args"].clone()));
                }
            }
            "GENERIC" => {
                let content = s["content"].as_str().unwrap_or("");
                if !extract_created_subagent_ids(content).is_empty() {
                    launched_sub = true;
                }
                let call = open_calls.pop_front();
                if s["status"].as_str() == Some("RUNNING") {
                    if let Some(parsed) = parse_agy_bg_task(content) {
                        let (_, is_daemon, _, _) = match call {
                            Some((name, args)) => agy_bg_meta(&name, &args, &parsed.description),
                            None => agy_bg_meta("", &Value::Null, &parsed.description),
                        };
                        if !is_daemon {
                            has_bg = true;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if !launched_sub && !has_bg {
        return false;
    }
    // Still pending unless a SYSTEM_MESSAGE already woke the parent and produced a finished empty-tool PLANNER_RESPONSE
    match slice.iter().rposition(is_agy_wakeup) {
        None => true,
        Some(wi) => !slice[wi + 1..].iter().any(is_agy_final_reply),
    }
}

#[derive(Debug, Clone)]
struct AgyBgTask {
    id: String,
    kind: String,
    description: String,
    started_at: i64,
    output_file: Option<String>,
    tc_id: Option<String>,
    is_daemon: bool,
    timer_cond: Option<String>,
    done: bool,
    announced: bool,
    result: Option<(String, bool)>,
}

impl AgyBgTask {
    fn to_proto(&self) -> BackgroundTask {
        BackgroundTask {
            id: self.id.clone(),
            kind: self.kind.clone(),
            description: if self.description.is_empty() { self.id.clone() } else { self.description.clone() },
            started_at: self.started_at,
            output_file: self.output_file.clone(),
            unstoppable: true,
        }
    }
}

#[derive(Debug, Clone)]
struct AgyLaneCall {
    id: String,
    name: String,
    args: Value,
}

fn settle_bg_tasks_with_system(tasks: &mut [AgyBgTask], step: &Value, out: &mut AgyOut, announce: bool) {
    if !is_agy_system(step) || step["status"].as_str() != Some("DONE") {
        return;
    }
    let content = step["content"].as_str().unwrap_or("");
    let Some(notice) = agy_task_notice(content) else {
        return;
    };
    let sender = match &notice {
        AgyTaskNotice::Finished { id, .. } | AgyTaskNotice::Canceled { id, .. } => id.as_str(),
        AgyTaskNotice::Message { sender, .. } => sender.as_str(),
    };
    for t in tasks.iter_mut() {
        if t.done {
            continue;
        }
        match &notice {
            AgyTaskNotice::Finished { id, output, is_error } if t.id == *id => {
                t.done = true;
                let truncated = crate::util::truncate(output, 64 * 1024);
                t.result = Some((truncated.clone(), *is_error));
                if let Some(tc_id) = t.tc_id.take() {
                    out.push(EventBody::ToolResult { id: tc_id, output: truncated, is_error: *is_error });
                }
                if announce && !t.announced {
                    t.announced = true;
                    let status = if *is_error { "后台任务失败" } else { "后台任务完成" };
                    let what = if t.description.is_empty() { &t.id } else { &t.description };
                    out.push(EventBody::System { text: format!("{status}：{what}") });
                }
            }
            AgyTaskNotice::Canceled { id, output } if t.id == *id => {
                t.done = true;
                let truncated = crate::util::truncate(output, 64 * 1024);
                t.result = Some((truncated.clone(), true));
                if let Some(tc_id) = t.tc_id.take() {
                    out.push(EventBody::ToolResult { id: tc_id, output: truncated, is_error: true });
                }
                if announce && !t.announced {
                    t.announced = true;
                    let what = if t.description.is_empty() { &t.id } else { &t.description };
                    out.push(EventBody::System { text: format!("后台任务已停止：{what}") });
                }
            }
            AgyTaskNotice::Message { sender: s, output } if t.id == *s => {
                t.done = true;
                let truncated = crate::util::truncate(output, 64 * 1024);
                t.result = Some((truncated.clone(), false));
                if let Some(tc_id) = t.tc_id.take() {
                    out.push(EventBody::ToolResult { id: tc_id, output: truncated, is_error: false });
                }
                if announce && !t.announced {
                    t.announced = true;
                    let what = if t.description.is_empty() { &t.id } else { &t.description };
                    out.push(EventBody::System { text: format!("后台任务完成：{what}") });
                }
            }
            _ => {
                if sender != "system" && (t.timer_cond.as_deref() == Some("any") || t.timer_cond.as_deref() == Some(sender)) {
                    t.done = true;
                    t.announced = true;
                    t.result = Some((String::new(), false));
                    if let Some(tc_id) = t.tc_id.take() {
                        out.push(EventBody::ToolResult { id: tc_id, output: String::new(), is_error: false });
                    }
                }
            }
        }
    }
}

/// Tool calls read from a transcript, waiting for the GENERIC steps that carry their results
#[derive(Debug, Default)]
struct AgyToolLane {
    /// Open tool calls in call order; GENERIC results come back in the same order
    open: Vec<AgyLaneCall>,
    /// Upcoming GENERIC steps that belong to calls we don't show (`send_message` back to the parent…)
    skip: usize,
    /// Background tasks (`run_command`, `run_workflow`, `schedule`): result arrives as a SYSTEM_MESSAGE
    background: Vec<AgyBgTask>,
}

impl AgyToolLane {
    fn busy(&self) -> bool {
        !self.open.is_empty() || self.background.iter().any(|b| !b.done)
    }

    fn push_open(&mut self, id: String, name: String, args: Value) {
        self.open.push(AgyLaneCall { id, name, args });
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
        let call = self.open.remove(0);
        let content = step["content"].as_str().unwrap_or("");
        if step["status"].as_str() == Some("RUNNING") {
            if let Some(parsed) = parse_agy_bg_task(content) {
                let (kind, is_daemon, timer_cond, desc) = agy_bg_meta(&call.name, &call.args, &parsed.description);
                if !is_daemon {
                    self.background.push(AgyBgTask {
                        id: parsed.id,
                        kind,
                        description: desc,
                        started_at: now_ms(),
                        output_file: parsed.output_file,
                        tc_id: Some(call.id),
                        is_daemon: false,
                        timer_cond,
                        done: false,
                        announced: false,
                        result: None,
                    });
                    return;
                }
            }
        }
        let out = clean_agy_generic_output(content, step["media"].as_array());
        watch_out.push(EventBody::ToolResult {
            id: call.id,
            output: crate::util::truncate(&out, 64 * 1024),
            is_error: matches!(step["status"].as_str(), Some("ERROR" | "INVALID")),
        });
    }

    /// A SYSTEM_MESSAGE: fills a background task's result when it reports finished/canceled
    fn system(&mut self, watch_out: &mut AgyOut, step: &Value) {
        settle_bg_tasks_with_system(&mut self.background, step, watch_out, false);
        self.background.retain(|b| !b.done);
    }

    fn close_all(&mut self, watch_out: &mut AgyOut) {
        for tc_id in self
            .open
            .drain(..)
            .map(|c| c.id)
            .chain(self.background.drain(..).filter(|b| !b.done).filter_map(|b| b.tc_id))
        {
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

/// Running subagents and background tasks handed from one watch to the next across a prompt
#[derive(Debug, Default)]
struct AgyCarry {
    subs: Vec<(String, AgySubagentTracker)>,
    calls: HashMap<String, AgyPendingCall>,
    bg_tasks: Vec<AgyBgTask>,
    unresumed_items: VecDeque<AgyWakeReplayItem>,
}

impl AgyCarry {
    fn is_empty(&self) -> bool {
        self.subs.is_empty() && self.bg_tasks.is_empty()
    }
}

/// What a pass of the watch decided
#[derive(Debug)]
enum AgyStep {
    /// Keep waiting
    Wait,
    /// An intermediate wakeup turn finished while other subagents or background tasks keep running
    CycleEnd,
    /// Finished; the request (if any) goes to agy
    Done(Option<Value>),
}

/// Antigravity subagents (`invoke_subagent`, `send_message` to one, `swarm`) and background tasks (`run_command`,
/// `run_workflow`, `schedule`) run asynchronously: agy ends the prompt turn after launching them, and when they report
/// back it wakes the parent by itself, outside any ACP prompt. Some versions stream ACP updates during a wakeup,
/// but there is no prompt response to finalize them. This follows the transcripts on disk. Running subagents and
/// background tasks are listed in `background_tasks`, and the parent's wakeup reopens the session (Running, then TurnEnd)
#[derive(Debug)]
struct AgyWatch {
    parent_sid: String,
    brain_dir: std::path::PathBuf,
    pre_turn_steps: usize,
    /// Steps in the parent's transcript when the prompt returned: everything before was streamed, so only a
    /// SYSTEM_MESSAGE after them can be the wakeup
    wake_from: usize,
    next_parent_discover_step: usize,
    discover_open: VecDeque<(String, Value)>,
    stop_reason: Option<String>,
    pending_calls: HashMap<String, AgyPendingCall>,
    open_bg_calls: VecDeque<(String, String)>,
    subagents: HashMap<String, AgySubagentTracker>,
    subagent_order: Vec<String>,
    bg_tasks: Vec<AgyBgTask>,
    seen_system_steps: HashSet<usize>,
    wakeup_msg_idx: Option<usize>,
    next_parent_emit_step: usize,
    parent_lane: AgyToolLane,
    parent: AgyTranscript,
    wakeup_stable_ticks: usize,
    started_at: tokio::time::Instant,
    last_acp_activity: Option<tokio::time::Instant>,
    /// Text already previewed from the current in-progress planner step
    preview: Option<(usize, String)>,
    all_subagents_done_at: Option<tokio::time::Instant>,
    /// Allow-all launches agy with --dangerously-skip-permissions, so subagents never wait for approval there
    stall_check: bool,
    /// The turn already ended (TurnEnd sent); the parent's wakeup reopens it with a Running status first
    detached: bool,
    reopened: bool,
    /// This pass produced the parent's own events (its wakeup), which reopen a detached turn
    wake: bool,
    wake_failed: bool,
    emitted_wake_text: bool,
    compacted: bool,
    unresumed_items: VecDeque<AgyWakeReplayItem>,
    out: AgyOut,
    settled_tc_ids: Vec<String>,
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
            discover_open: VecDeque::new(),
            stop_reason,
            pending_calls,
            open_bg_calls: VecDeque::new(),
            subagents: HashMap::new(),
            subagent_order: vec![],
            bg_tasks: vec![],
            seen_system_steps: HashSet::new(),
            wakeup_msg_idx: None,
            next_parent_emit_step: pre_turn_steps,
            parent_lane: AgyToolLane::default(),
            parent: AgyTranscript::default(),
            wakeup_stable_ticks: 0,
            started_at: tokio::time::Instant::now(),
            last_acp_activity: None,
            preview: None,
            all_subagents_done_at: None,
            stall_check,
            detached: false,
            reopened: false,
            wake: false,
            wake_failed: false,
            emitted_wake_text: false,
            compacted: false,
            unresumed_items: VecDeque::new(),
            out: vec![],
            settled_tc_ids: vec![],
        };
        for (sid, baseline) in send_baselines {
            watch.add_subagent(sid, Some(baseline));
        }
        watch
    }

    fn with_open_bg_calls(mut self, calls: VecDeque<(String, String)>) -> Self {
        self.open_bg_calls = calls;
        self
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

    fn add_bg_task(&mut self, task: AgyBgTask) {
        if self.bg_tasks.iter().any(|b| b.id == task.id) {
            return;
        }
        if !task.done && !task.is_daemon {
            self.all_subagents_done_at = None;
        }
        self.bg_tasks.push(task);
    }

    /// Take over subagents and background tasks still running from the previous watch
    fn adopt(&mut self, carry: AgyCarry) {
        for (id, t) in carry.subs {
            if !self.subagents.contains_key(&id) {
                self.subagent_order.push(id.clone());
                self.subagents.insert(id, t);
                self.all_subagents_done_at = None;
            }
        }
        self.pending_calls.extend(carry.calls);
        for b in carry.bg_tasks {
            self.add_bg_task(b);
        }
        self.unresumed_items.extend(carry.unresumed_items);
    }

    fn adopt_bg(&mut self, bg_tasks: Vec<AgyBgTask>, seen_system_steps: HashSet<usize>) {
        self.seen_system_steps.extend(seen_system_steps);
        for b in bg_tasks {
            self.add_bg_task(b);
        }
    }

    /// Running subagents and background tasks
    fn tasks(&mut self) -> Vec<BackgroundTask> {
        let mut v = vec![];
        let mut seen = HashSet::new();
        for id in &self.subagent_order {
            let Some(t) = self.subagents.get_mut(id) else { continue };
            if t.desc.is_empty() {
                if let Some(c) = self.pending_calls.values().find(|c| c.target_subagent.as_deref() == Some(id.as_str())) {
                    t.desc = c.desc.clone();
                }
            }
            if !t.done && seen.insert(id.clone()) {
                v.push(BackgroundTask {
                    id: id.clone(),
                    kind: "local_agent".into(),
                    description: if t.desc.is_empty() { id.chars().take(8).collect() } else { t.desc.clone() },
                    started_at: t.started_at,
                    output_file: None,
                    unstoppable: false,
                });
            }
            for b in &t.lane.background {
                if !b.done && !b.is_daemon && seen.insert(b.id.clone()) {
                    v.push(b.to_proto());
                }
            }
        }
        for b in &self.bg_tasks {
            if !b.done && !b.is_daemon && seen.insert(b.id.clone()) {
                v.push(b.to_proto());
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

    fn reply_id(&self, step: &Value, index: usize) -> String {
        let step_id = step["step_index"].as_u64().unwrap_or(index as u64);
        format!("agy-wake-{}-a-{step_id}", self.parent_sid)
    }

    fn thinking_id(&self, step: &Value, index: usize) -> String {
        let step_id = step["step_index"].as_u64().unwrap_or(index as u64);
        format!("agy-wake-{}-t-{step_id}", self.parent_sid)
    }

    fn preview_reply(&mut self, step: &Value, index: usize) {
        if step["type"] != "PLANNER_RESPONSE" {
            return;
        }
        let Some(text) = step["content"].as_str().filter(|text| !text.trim().is_empty()) else { return };
        let previous = self.preview.as_ref().filter(|(i, _)| *i == index).map_or("", |(_, text)| text.as_str());
        if let Some(delta) = text.strip_prefix(previous).filter(|delta| !delta.is_empty()) {
            self.out.push(EventBody::AssistantDelta { id: self.reply_id(step, index), delta: delta.to_string() });
            self.preview = Some((index, text.to_string()));
        }
    }

    /// Send what this pass produced. A detached turn is reopened (status Running) before the parent's wakeup;
    /// the subagents' own events go out while the session is idle, like a background task's
    fn flush_out(&mut self, em: &Emitter) {
        if self.detached && !self.reopened && self.wake {
            self.reopened = true;
            em.status(SessionStatus::Running, None);
        }
        for body in self.out.drain(..) {
            if let EventBody::ToolResult { id, .. } = &body {
                self.settled_tc_ids.push(id.clone());
            }
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

fn finish_agy_watch(watch: &mut AgyWatch) {
    flush_ready_pending_calls(watch, true);
    for sid in watch.subagent_order.clone() {
        if let Some(t) = watch.subagents.get_mut(&sid) {
            t.lane.close_all(&mut watch.out);
        }
    }
    watch.parent_lane.close_all(&mut watch.out);
    for b in &mut watch.bg_tasks {
        if !b.done {
            if let Some(tc_id) = b.tc_id.take() {
                watch.out.push(EventBody::ToolResult { id: tc_id, output: String::new(), is_error: false });
            }
        }
    }
    for (tc_id, _) in watch.open_bg_calls.drain(..) {
        watch.out.push(EventBody::ToolResult { id: tc_id, output: String::new(), is_error: false });
    }
}

/// Stop following the subagents and background tasks (the adapter exits)
fn drop_agy_watch(st: &mut State, em: &Emitter) {
    if let Some(mut watch) = st.agy_watch.take() {
        finish_agy_watch(&mut watch);
        for body in watch.out {
            em.exit_cleanup_event(body);
        }
        em.watch_active(false);
        em.background_tasks(vec![]);
    }
}

/// End the watch but keep its running subagents and background tasks in `st.agy_carry`:
/// a new prompt is going out (the parent's own steps would confuse the watch), or the wakeup turn was interrupted
fn carry_agy_watch(st: &mut State, em: &Emitter) {
    let Some(mut watch) = st.agy_watch.take() else {
        return;
    };
    let parent_steps = watch.parent.refresh(&watch.brain_dir, &watch.parent_sid);
    for (idx, step) in parent_steps.iter().enumerate().skip(watch.pre_turn_steps) {
        if is_agy_system(step) && step["status"].as_str() == Some("DONE") && watch.seen_system_steps.insert(idx) {
            settle_bg_tasks_with_system(&mut watch.bg_tasks, step, &mut watch.out, true);
        }
    }
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
    for b in watch.bg_tasks.drain(..) {
        if !b.done && !b.is_daemon {
            st.agy_carry.bg_tasks.push(b);
        }
    }
    st.agy_carry.unresumed_items.extend(watch.unresumed_items.drain(..));
    finish_agy_watch(&mut watch);
    watch.flush_out(em);
    mark_watch_settled(st, None, em, &mut watch);
    em.watch_active(false);
    em.background_tasks(current_agy_tasks(st));
}

fn mark_watch_settled(st: &mut State, spec: Option<&StartSpec>, em: &Emitter, watch: &mut AgyWatch) {
    for id in watch.settled_tc_ids.drain(..) {
        if let Some(t) = st.tools.get_mut(&id) {
            t.done = true;
        }
        st.agy_open_bg_calls.retain(|(x, _)| x != &id);
    }
    if std::mem::take(&mut watch.compacted) {
        rearm_memory_notice(st, spec, em);
    }
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
    let mut v: Vec<BackgroundTask> = c
        .subs
        .iter()
        .map(|(id, t)| BackgroundTask {
            id: id.clone(),
            kind: "local_agent".into(),
            description: if t.desc.is_empty() { id.chars().take(8).collect() } else { t.desc.clone() },
            started_at: t.started_at,
            output_file: None,
            unstoppable: false,
        })
        .collect();
    for b in &c.bg_tasks {
        if !b.done && !b.is_daemon {
            v.push(b.to_proto());
        }
    }
    v
}

fn current_agy_tasks(st: &State) -> Vec<BackgroundTask> {
    let mut v = em_carry_tasks(&st.agy_carry);
    let mut seen: HashSet<String> = v.iter().map(|t| t.id.clone()).collect();
    for b in &st.agy_bg_tasks {
        if !b.done && !b.is_daemon && seen.insert(b.id.clone()) {
            v.push(b.to_proto());
        }
    }
    v
}

fn settle_agy_bg_from_steps(st: &mut State, steps: &[Value], em: &Emitter) {
    let mut out = vec![];
    for (idx, step) in steps.iter().enumerate().skip(st.agy_pre_turn_steps) {
        if is_agy_system(step) && step["status"].as_str() == Some("DONE") && st.agy_seen_system_steps.insert(idx) {
            settle_bg_tasks_with_system(&mut st.agy_bg_tasks, step, &mut out, true);
            settle_bg_tasks_with_system(&mut st.agy_carry.bg_tasks, step, &mut out, true);
        }
    }
    if !out.is_empty() {
        for body in out {
            if let EventBody::ToolResult { id, .. } = &body {
                if let Some(t) = st.tools.get_mut(id) {
                    t.done = true;
                }
                st.agy_open_bg_calls.retain(|(x, _)| x != id);
            }
            em.event(body);
        }
        st.agy_carry.bg_tasks.retain(|b| !b.done);
        em.background_tasks(current_agy_tasks(st));
    }
}

fn take_agy_open_bg_call(
    open: &mut VecDeque<(String, String)>,
    tools: Option<&HashMap<String, ToolState>>,
    call_name: Option<&str>,
    call_args: Option<&Value>,
) -> Option<String> {
    let cn = call_name?;
    if let (Some(tools), Some(args)) = (tools, call_args) {
        let want_cmd = arg_str(&args["CommandLine"]);
        let want_sum = arg_str(&args["toolSummary"]);
        if let Some(pos) = open.iter().position(|(id, n)| {
            n.eq_ignore_ascii_case(cn)
                && tools.get(id).is_some_and(|t| {
                    t.input == *args
                        || (want_cmd.is_some() && arg_str(&t.input["CommandLine"]) == want_cmd)
                        || (want_sum.is_some() && arg_str(&t.input["toolSummary"]) == want_sum)
                })
        }) {
            return open.remove(pos).map(|(id, _)| id);
        }
    }
    open.iter()
        .position(|(_, n)| n.eq_ignore_ascii_case(cn))
        .and_then(|pos| open.remove(pos))
        .map(|(id, _)| id)
}

fn discover_agy_bg_from_steps(st: &mut State, steps: &[Value]) -> bool {
    let mut added = false;
    let mut open_calls: VecDeque<(String, Value)> = VecDeque::new();
    for step in steps.iter().skip(st.agy_pre_turn_steps) {
        if !agy_step_settled(step) {
            if step["type"].as_str() == Some("GENERIC") {
                let _ = open_calls.pop_front();
                continue;
            }
            break;
        }
        match step["type"].as_str().unwrap_or("") {
            "PLANNER_RESPONSE" => {
                open_calls.clear();
                for tc in step["tool_calls"].as_array().into_iter().flatten() {
                    open_calls.push_back((tc["name"].as_str().unwrap_or("").to_string(), tc["args"].clone()));
                }
            }
            "GENERIC" => {
                let call = open_calls.pop_front();
                if step["status"].as_str() == Some("RUNNING") {
                    if let Some(parsed) = parse_agy_bg_task(step["content"].as_str().unwrap_or("")) {
                        if !st.agy_bg_tasks.iter().any(|b| b.id == parsed.id) {
                            let call_name = call.as_ref().map(|(n, _)| n.clone());
                            let call_args = call.as_ref().map(|(_, a)| a.clone());
                            let (kind, is_daemon, timer_cond, desc) = match call {
                                Some((name, args)) => agy_bg_meta(&name, &args, &parsed.description),
                                None => agy_bg_meta("", &Value::Null, &parsed.description),
                            };
                            let tc_id = take_agy_open_bg_call(
                                &mut st.agy_open_bg_calls,
                                Some(&st.tools),
                                call_name.as_deref(),
                                call_args.as_ref(),
                            );
                            if !is_daemon {
                                st.agy_bg_tasks.push(AgyBgTask {
                                    id: parsed.id,
                                    kind,
                                    description: desc,
                                    started_at: now_ms(),
                                    output_file: parsed.output_file,
                                    tc_id,
                                    is_daemon: false,
                                    timer_cond,
                                    done: false,
                                    announced: false,
                                    result: None,
                                });
                                added = true;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    added
}

fn poll_agy_prompt_bg(st: &mut State, em: &Emitter) {
    let Some(sid) = st.session_id.as_deref() else {
        return;
    };
    let steps = st.agy_parent_transcript.refresh(&agy_brain_dir(), sid);
    if discover_agy_bg_from_steps(st, &steps) {
        em.background_tasks(current_agy_tasks(st));
    }
    settle_agy_bg_from_steps(st, &steps, em);
}

fn settle_agy_prompt_bg(st: &mut State, em: &Emitter) {
    let Some(sid) = st.session_id.as_deref() else {
        return;
    };
    let steps = st.agy_parent_transcript.refresh(&agy_brain_dir(), sid);
    discover_agy_bg_from_steps(st, &steps);
    settle_agy_bg_from_steps(st, &steps, em);
    st.agy_carry.bg_tasks.retain(|b| !b.done);
    em.background_tasks(current_agy_tasks(st));
}

fn agy_resume_req(watch: &AgyWatch, spec: &StartSpec, next_id: &mut u64, pending: &mut HashMap<u64, Pending>) -> Value {
    // agy only marks steps it streamed as seen; resuming moves that mark past the wakeup turn, so the
    // next prompt doesn't replay it (session/resume sends no history)
    *next_id += 1;
    pending.insert(*next_id, Pending::AgyResume);
    json!({
        "jsonrpc": "2.0",
        "id": *next_id,
        "method": "session/resume",
        "params": {"sessionId": watch.parent_sid, "cwd": spec.cwd.display().to_string(), "mcpServers": spec.mcp_servers}
    })
}

fn remember_agy_wake_progress(st: &mut State, watch: &AgyWatch) {
    st.agy_wake_from = st.agy_wake_from.max(watch.wake_from);
    if watch.wake && watch.wakeup_msg_idx.is_some_and(|idx| watch.next_parent_emit_step > idx + 1) {
        st.agy_wake_from = st.agy_wake_from.max(watch.next_parent_emit_step);
    }
}

fn agy_wake_end(watch: &AgyWatch) -> EventBody {
    let reason = if watch.wake_failed { "error" } else { "end_turn" };
    EventBody::TurnEnd { stop_reason: Some(reason.into()), model: None }
}

fn poll_agy_watch(st: &mut State, spec: &StartSpec, em: &Emitter) -> Option<Value> {
    let mut watch = st.agy_watch.take()?;
    if !st.perms.is_empty() || !st.asks.is_empty() || !st.elicits.is_empty() {
        watch.started_at = tokio::time::Instant::now();
        watch.all_subagents_done_at = None;
        watch.reopened |= watch.detached;
    }
    let step = step_agy_watch(&mut watch, spec, em, &mut st.next_id, &mut st.pending);
    mark_watch_settled(st, Some(spec), em, &mut watch);
    match step {
        AgyStep::Wait => {
            st.agy_watch = Some(watch);
            None
        }
        AgyStep::CycleEnd => {
            remember_agy_wake_progress(st, &watch);
            if watch.reopened {
                watch.reopened = false;
                st.end_subagents();
                em.event(agy_wake_end(&watch));
            }
            watch.wake = false;
            watch.wake_failed = false;
            st.agy_watch = Some(watch);
            None
        }
        AgyStep::Done(req) => {
            remember_agy_wake_progress(st, &watch);
            // A queued prompt can start before the resume RPC is acknowledged or after it fails.
            st.agy_carry.unresumed_items.append(&mut watch.unresumed_items);
            if !watch.detached {
                st.prompting = false;
                st.end_subagents();
                em.event(EventBody::TurnEnd { stop_reason: watch.stop_reason, model: None });
            } else if watch.reopened {
                st.end_subagents();
                em.event(agy_wake_end(&watch));
            }
            req
        }
    }
}

/// Advance the Antigravity subagent + background-task + parent-wakeup watch by one tick
fn step_agy_watch(
    watch: &mut AgyWatch,
    spec: &StartSpec,
    em: &Emitter,
    next_id: &mut u64,
    pending: &mut HashMap<u64, Pending>,
) -> AgyStep {
    let step = step_agy_watch_inner(watch, spec, next_id, pending);
    let done = matches!(step, AgyStep::Done(_));
    if done {
        // Close whatever is still open (Task calls without a report, subagent tool calls without a result)
        finish_agy_watch(watch);
    }
    watch.flush_out(em);
    // Keep watch_active true across the gap between the last task finishing and the parent's wakeup turn so
    // deferred restarts / idle release / host account switches don't kill agy before it can wake up.
    em.watch_active(!done);
    em.background_tasks(if done { vec![] } else { watch.tasks() });
    step
}

fn step_agy_watch_inner(watch: &mut AgyWatch, spec: &StartSpec, next_id: &mut u64, pending: &mut HashMap<u64, Pending>) -> AgyStep {
    use std::time::Duration;
    /// Give up on subagents/tasks whose transcripts (and the parent's) haven't moved for this long
    const IDLE_TIMEOUT: Duration = Duration::from_secs(1800);
    const DISCOVER_GRACE: Duration = Duration::from_secs(3);
    const WAKEUP_GRACE: Duration = Duration::from_secs(20);
    /// Upper bound for waiting on the first PLANNER_RESPONSE after a wakeup SYSTEM_MESSAGE arrives
    const WAKEUP_START_TIMEOUT: Duration = Duration::from_secs(300);
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
            .chain(watch.last_acp_activity)
            .chain(std::iter::once(watch.started_at))
            .max()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    };
    let timed_out = idle_for() >= IDLE_TIMEOUT;

    // 1. Discover newly spawned subagents and background tasks from parent transcript steps since pre_turn_steps
    while watch.next_parent_discover_step < parent_steps.len() {
        let step = &parent_steps[watch.next_parent_discover_step];
        if !agy_step_settled(step) {
            break;
        }
        match step["type"].as_str().unwrap_or("") {
            "PLANNER_RESPONSE" => {
                watch.discover_open.clear();
                for tc in step["tool_calls"].as_array().into_iter().flatten() {
                    let name = tc["name"].as_str().unwrap_or("");
                    if name == "send_message" {
                        if let Some(recip) = agy_recipient(&tc["args"]) {
                            watch.add_subagent(recip.to_string(), None);
                        }
                    }
                    watch.discover_open.push_back((name.to_string(), tc["args"].clone()));
                }
            }
            "GENERIC" => {
                let call = watch.discover_open.pop_front();
                let content = step["content"].as_str().unwrap_or("");
                for sub_id in extract_created_subagent_ids(content) {
                    watch.add_subagent(sub_id.clone(), Some(0));
                    // Bind an unbound invoke_subagent call to the first subagent it created
                    if let Some(call) = watch.pending_calls.values_mut().find(|c| c.target_subagent.is_none() && c.name.eq_ignore_ascii_case("invoke_subagent")) {
                        call.target_subagent = Some(sub_id.clone());
                    }
                }
                if step["status"].as_str() == Some("RUNNING") {
                    if let Some(parsed) = parse_agy_bg_task(content) {
                        if !watch.bg_tasks.iter().any(|b| b.id == parsed.id) {
                            let call_name = call.as_ref().map(|(n, _)| n.clone());
                            let (kind, is_daemon, timer_cond, desc) = match call {
                                Some((name, args)) => agy_bg_meta(&name, &args, &parsed.description),
                                None => agy_bg_meta("", &Value::Null, &parsed.description),
                            };
                            let tc_id = take_agy_open_bg_call(&mut watch.open_bg_calls, None, call_name.as_deref(), None);
                            if !is_daemon {
                                watch.add_bg_task(AgyBgTask {
                                    id: parsed.id,
                                    kind,
                                    description: desc,
                                    started_at: now_ms(),
                                    output_file: parsed.output_file,
                                    tc_id,
                                    is_daemon: false,
                                    timer_cond,
                                    done: false,
                                    announced: false,
                                    result: None,
                                });
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        watch.next_parent_discover_step += 1;
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
                                watch.out.push(EventBody::ToolCall { id: tc_id.clone(), name: tc_name.to_string(), title: tc_name.to_string(), input: tc_args.clone(), diffs: vec![] });
                                tracker.lane.push_open(tc_id, tc_name.to_string(), tc_args);
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

    // 3. Check parent transcript for SYSTEM_MESSAGE notifications from finished subagent(s) and background tasks
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
        if watch.seen_system_steps.insert(idx) {
            settle_bg_tasks_with_system(&mut watch.bg_tasks, step, &mut watch.out, true);
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

    // 4. Stream & capture parent's reactive wakeup turn once SYSTEM_MESSAGE arrives (even while other tasks still run)
    let mut has_planner_after_wakeup = false;
    if let Some(after_wakeup) = watch.wakeup_msg_idx.and_then(|idx| parent_steps.get(idx + 1..)) {
        has_planner_after_wakeup = after_wakeup.iter().any(|s| s["type"] == "PLANNER_RESPONSE");
        // Only reopen for a new parent wakeup. Delayed ACP chunks from a completed step can
        // re-arm the watch, but cannot reopen that step or create an unmatched live buffer.
        watch.wake |= has_planner_after_wakeup || watch.last_acp_activity.is_some();
        while watch.next_parent_emit_step < parent_steps.len() {
            let step_idx = watch.next_parent_emit_step;
            let step = &parent_steps[step_idx];
            if !agy_step_settled(step) {
                watch.preview_reply(step, step_idx);
                watch.wakeup_stable_ticks = 0;
                break;
            }
            let emitted = match step["type"].as_str().unwrap_or("") {
                "PLANNER_RESPONSE" => {
                    if let Some(thinking) = step["thinking"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        watch.unresumed_items.push_back(AgyWakeReplayItem::Thinking(thinking.to_string()));
                        watch.out.push(EventBody::Thinking { id: watch.thinking_id(step, step_idx), text: thinking.to_string() });
                    }
                    for (tc_idx, tc) in step["tool_calls"].as_array().into_iter().flatten().enumerate() {
                        let tc_name = tc["name"].as_str().unwrap_or("tool");
                        let tc_args = tc["args"].clone();
                        let tc_id = tc["id"].as_str().map(str::to_string).unwrap_or_else(|| format!("agy-wake:{}:{}:{}", watch.parent_sid, step_idx, tc_idx));
                        watch.unresumed_items.push_back(AgyWakeReplayItem::Tool { name: tc_name.to_string(), input: tc_args.clone() });
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
                            watch.parent_lane.push_open(tc_id, tc_name.to_string(), tc_args);
                        }
                    }
                    if let Some(text) = step["content"].as_str().filter(|s| !s.trim().is_empty()) {
                        watch.emitted_wake_text = true;
                        watch.unresumed_items.push_back(AgyWakeReplayItem::Assistant(text.trim().to_string()));
                        watch.out.push(EventBody::AssistantText { id: watch.reply_id(step, step_idx), text: text.to_string() });
                    }
                    true
                }
                "GENERIC" => {
                    let content = step["content"].as_str().unwrap_or("");
                    for sub_id in extract_created_subagent_ids(content) {
                        if let Some(call) = watch.pending_calls.values_mut().find(|c| c.target_subagent.is_none() && c.name.eq_ignore_ascii_case("invoke_subagent")) {
                            call.target_subagent = Some(sub_id.clone());
                        }
                        watch.add_subagent(sub_id, Some(0));
                    }
                    if watch.parent_lane.skip > 0 {
                        watch.parent_lane.skip -= 1;
                    } else if let Some(call) = (!watch.parent_lane.open.is_empty()).then(|| watch.parent_lane.open.remove(0)) {
                        if step["status"].as_str() == Some("RUNNING") {
                            if let Some(parsed) = parse_agy_bg_task(content) {
                                let (kind, is_daemon, timer_cond, desc) = agy_bg_meta(&call.name, &call.args, &parsed.description);
                                if is_daemon {
                                    let out = clean_agy_generic_output(content, step["media"].as_array());
                                    watch.out.push(EventBody::ToolResult { id: call.id, output: crate::util::truncate(&out, 64 * 1024), is_error: false });
                                } else if let Some(b) = watch.bg_tasks.iter_mut().find(|b| b.id == parsed.id) {
                                    if b.done {
                                        let (out, is_error) = b.result.clone().unwrap_or_default();
                                        watch.out.push(EventBody::ToolResult { id: call.id, output: out, is_error });
                                    } else {
                                        b.tc_id.get_or_insert(call.id);
                                    }
                                } else {
                                    watch.add_bg_task(AgyBgTask {
                                        id: parsed.id,
                                        kind,
                                        description: desc,
                                        started_at: now_ms(),
                                        output_file: parsed.output_file,
                                        tc_id: Some(call.id),
                                        is_daemon: false,
                                        timer_cond,
                                        done: false,
                                        announced: false,
                                        result: None,
                                    });
                                }
                            } else {
                                let out = clean_agy_generic_output(content, step["media"].as_array());
                                watch.out.push(EventBody::ToolResult { id: call.id, output: crate::util::truncate(&out, 64 * 1024), is_error: false });
                            }
                        } else {
                            let out = clean_agy_generic_output(content, step["media"].as_array());
                            watch.out.push(EventBody::ToolResult {
                                id: call.id,
                                output: crate::util::truncate(&out, 64 * 1024),
                                is_error: matches!(step["status"].as_str(), Some("ERROR" | "INVALID")),
                            });
                        }
                    }
                    true
                }
                "CHECKPOINT" => {
                    if is_agy_checkpoint(step) {
                        let pre_tokens = agy_last_step_tokens(&parent_steps[..step_idx])
                            .or_else(|| agy_step_input_tokens(step));
                        let post_tokens = agy_post_checkpoint_tokens(&parent_steps[step_idx + 1..]);
                        watch.out.push(EventBody::Compact {
                            phase: "done".into(),
                            trigger: Some("auto".into()),
                            pre_tokens,
                            post_tokens,
                            duration_ms: None,
                        });
                        watch.compacted = true;
                        true
                    } else {
                        false
                    }
                }
                "SYSTEM_MESSAGE" => false,
                _ => false,
            };
            watch.next_parent_emit_step += 1;
            watch.wakeup_stable_ticks = 0;
            if emitted {
                watch.wake = true;
            }
        }

        let all_done = watch.subagents.values().all(|t| t.done) && watch.bg_tasks.iter().all(|b| b.done);
        let last = after_wakeup.iter().rev().find(|step| !is_agy_system(step));
        let failed = last.is_some_and(|s| {
            s["type"] == "PLANNER_RESPONSE"
                && matches!(s["status"].as_str(), Some("ERROR" | "INVALID"))
                && s["tool_calls"].as_array().is_none_or(|a| a.is_empty())
        });
        let last_is_final_planner = failed || last.is_some_and(is_agy_final_reply);
        if has_planner_after_wakeup && last_is_final_planner && !watch.parent_lane.busy() && watch.next_parent_emit_step == parent_steps.len() {
            watch.wakeup_stable_ticks += 1;
            if watch.wakeup_stable_ticks >= 2 {
                watch.wake_failed = failed;
                if all_done {
                    flush_ready_pending_calls(watch, true);
                    return AgyStep::Done(Some(agy_resume_req(watch, spec, next_id, pending)));
                }
                // Another background task or subagent is still running: close this intermediate wakeup cycle and keep watching
                watch.wake_from = parent_steps.len();
                watch.wakeup_msg_idx = None;
                watch.next_parent_emit_step = parent_steps.len();
                watch.wakeup_stable_ticks = 0;
                watch.all_subagents_done_at = None;
                watch.last_acp_activity = None;
                watch.preview = None;
                return AgyStep::CycleEnd;
            }
            return AgyStep::Wait;
        }
    }

    if watch.subagents.is_empty() && watch.bg_tasks.is_empty() && watch.wakeup_msg_idx.is_none() && !watch.reopened {
        let expired = match watch.last_acp_activity {
            Some(activity) => activity.elapsed() >= WAKEUP_GRACE,
            None => watch.started_at.elapsed() >= DISCOVER_GRACE,
        };
        return if expired {
            let need_resume = !watch.unresumed_items.is_empty();
            AgyStep::Done(need_resume.then(|| agy_resume_req(watch, spec, next_id, pending)))
        } else {
            AgyStep::Wait
        };
    }

    // 5. Wait until all tracked subagents and background tasks are finished
    if !watch.subagents.values().all(|t| t.done) || !watch.bg_tasks.iter().all(|b| b.done) {
        return if timed_out {
            watch.wake_failed = true;
            AgyStep::Done(None)
        } else {
            AgyStep::Wait
        };
    }

    let done_at = *watch.all_subagents_done_at.get_or_insert_with(tokio::time::Instant::now);

    // A real wakeup message can precede the first model step by more than WAKEUP_GRACE (e.g. a slow model call).
    // Wait up to WAKEUP_START_TIMEOUT before concluding no reply will arrive, instead of stalling for IDLE_TIMEOUT.
    let no_unsettled = !parent_steps.get(watch.wake_from..).is_some_and(|tail| tail.iter().any(|s| !agy_step_settled(s)));
    let acp_quiet = watch.last_acp_activity.is_none_or(|t| t.elapsed() >= WAKEUP_GRACE);
    let grace_exceeded = no_unsettled
        && ((watch.wakeup_msg_idx.is_none() && done_at.elapsed() >= WAKEUP_GRACE)
            || (watch.wakeup_msg_idx.is_some()
                && !has_planner_after_wakeup
                && acp_quiet
                && done_at.elapsed() >= WAKEUP_START_TIMEOUT));

    if grace_exceeded || timed_out {
        let emitted_wakeup_text = watch.emitted_wake_text
            || watch.wakeup_msg_idx.is_some_and(|wi| {
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
        if timed_out && (watch.wake || watch.reopened) {
            watch.wake_failed = true;
            return AgyStep::Done(None);
        }
        let need_resume = watch.wake || !watch.unresumed_items.is_empty();
        return AgyStep::Done(need_resume.then(|| agy_resume_req(watch, spec, next_id, pending)));
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
        let mut st = State { browser_notice: true, ..Default::default() };
        let first = texts(&prompt(&mut st, &em, &ask("check the page")));
        assert!(first[0].contains("yi_browser") && first[0].contains("YI_BROWSER_PAGE_CONTENT"), "{first:?}");
        assert_eq!(texts(&prompt(&mut st, &em, &ask("again"))), ["again"]);
    }

    #[test]
    fn gateway_permissions_are_recognized_only_in_exact_forms() {
        use super::*;
        let none = Value::Null;
        assert_eq!(gateway_tool("mcp__yi_browser__browser_click", "", &none).as_deref(), Some("mcp__yi_browser__browser_click"));
        assert_eq!(gateway_tool("tool", "browser_snapshot (yi_browser)", &none).as_deref(), Some("mcp__yi_browser__browser_snapshot"));
        let wrapped = json!({"toolName": "mcp__yi_browser__browser_open", "params": {"url": "x"}});
        assert_eq!(gateway_tool("DeferExecuteTool", "DeferExecuteTool", &wrapped).as_deref(), Some("mcp__yi_browser__browser_open"));
        // Unknown tools, other servers, and anything that only mentions the name are left to the user
        assert_eq!(gateway_tool("mcp__yi_browser__browser_nope", "", &none), None);
        assert_eq!(gateway_tool("mcp__yi_computer_use__click", "", &none), None);
        assert_eq!(gateway_tool("Bash", "echo browser_click (yi_browser) && rm -rf x", &json!({"command": "mcp__yi_browser__browser_click"})), None);
        assert_eq!(gateway_tool("Bash", "Bash", &json!({"toolName": "mcp__yi_browser__browser_click"})), None);
        // yi_memory exact forms
        assert_eq!(gateway_tool("mcp__yi_memory__memory_read", "", &none).as_deref(), Some("mcp__yi_memory__memory_read"));
        assert_eq!(gateway_tool("tool", "memory_search (yi_memory)", &none).as_deref(), Some("mcp__yi_memory__memory_search"));
        let mem_wrapped = json!({"toolName": "mcp__yi_memory__memory_save", "params": {"id": "x"}});
        assert_eq!(gateway_tool("DeferExecuteTool", "DeferExecuteTool", &mem_wrapped).as_deref(), Some("mcp__yi_memory__memory_save"));
        assert_eq!(gateway_tool("mcp__yi_memory__memory_nope", "", &none), None);
        assert_eq!(gateway_tool("Bash", "echo memory_read (yi_memory)", &json!({"command": "mcp__yi_memory__memory_read"})), None);
        // yi_session exact forms
        assert_eq!(gateway_tool("mcp__yi_session__session_read", "", &none).as_deref(), Some("mcp__yi_session__session_read"));
        assert_eq!(gateway_tool("tool", "session_list_related (yi_session)", &none).as_deref(), Some("mcp__yi_session__session_list_related"));
        let sess_wrapped = json!({"toolName": "mcp__yi_session__session_read", "params": {"sessionId": "p"}});
        assert_eq!(gateway_tool("DeferExecuteTool", "DeferExecuteTool", &sess_wrapped).as_deref(), Some("mcp__yi_session__session_read"));
        assert_eq!(gateway_tool("mcp__yi_session__session_write", "", &none), None);
        assert_eq!(gateway_tool("Bash", "echo session_read (yi_session)", &json!({"command": "mcp__yi_session__session_read"})), None);
    }

    #[test]
    fn browser_permission_is_answered_only_while_the_browser_is_on() {
        use super::*;
        let (hub, em, _rx) = crate::hub::test_emitter();
        let req = json!({"jsonrpc": "2.0", "id": 7, "method": "session/request_permission", "params": {
            "toolCall": {"toolCallId": "c1", "title": "browser_click (yi_browser)", "rawInput": {"ref": "e1"}},
            "options": [{"optionId": "once", "name": "Allow", "kind": "allow_once"}, {"optionId": "no", "name": "Reject", "kind": "reject_once"}]}});
        let mut st = State { perm: "default".into(), ..Default::default() };
        // Off: the user decides
        let out = handle(&mut st, &super::model_config_tests::agy_test_spec(), &req, &em).unwrap();
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(st.perms.len(), 1);
        // On: answered with allow once
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("chrome");
        std::fs::write(&exe, b"").unwrap();
        let mut settings = yi_proto::DaemonSettings::default();
        settings.browser.executable_path = Some(exe.to_string_lossy().into_owned());
        std::env::set_var("YI_CONSOLE_HOME", std::env::temp_dir().join(format!("yi-acp-test-{}", std::process::id())));
        hub.browsers.configure("s", &settings).unwrap();
        let mut st = State { perm: "default".into(), ..Default::default() };
        let out = handle(&mut st, &super::model_config_tests::agy_test_spec(), &req, &em).unwrap();
        assert_eq!(out[0]["result"]["outcome"]["optionId"], "once");
        // Paused: the user decides again
        hub.browsers.change("s", "pause").unwrap();
        let mut st = State { perm: "default".into(), ..Default::default() };
        assert!(handle(&mut st, &super::model_config_tests::agy_test_spec(), &req, &em).unwrap().is_empty());
        hub.browsers.change("s", "stop").unwrap();
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
            mcp_servers: vec![], mcp_fingerprint: String::new(), allow_mcp: vec![], system_prompt: None, system_notice: false, memory_prompt: None, memory_notice: false, cols: 80, rows: 24, tmux: None, session_id: None,
            env: Default::default(), env_unset: vec![], data_dir: None, auth_method: None, account_handoff: None, fork_session: false, fork_fallback_handoff: None,
        }
    }

    #[test]
    fn cursor_compact_summarizes_into_a_fresh_session() {
        let (hub, em, _rx) = crate::hub::test_emitter();
        crate::hub::test_set_tool(&hub, "s", ToolKind::Cursor);
        let mut spec = cursor_spec("grok-4.7");
        spec.system_prompt = Some("be brief".into());
        let mut st = State { is_cursor: true, parameterized: true, session_id: Some("old".into()), config_model: Some("gpt-6".into()), ..Default::default() };
        st.config_options = vec![ModelConfigOption { id: "fast".into(), name: "Fast".into(), category: "model_config".into(), description: String::new(),
            current_value: "true".into(), options: vec![] }];
        // Context the hub put first (an account hand-off) goes into the summary request
        let req = prompt(&mut st, &em, &Prompt { text: "<yi-account-handoff>\nUser: fix x\n</yi-account-handoff>\n\n/compact keep paths".into(), images: vec![] });
        let sent = req["params"]["prompt"][0]["text"].as_str().unwrap();
        assert!(sent.contains("Write a summary") && sent.contains("User: fix x") && sent.ends_with("keep paths"), "{sent}");
        assert_eq!(req["params"]["prompt"].as_array().unwrap().len(), 1);
        assert!(st.compacting.is_some());
        // The summary is hidden from the chat
        update(&mut st, &em, &json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Goal: fix x."}}));
        let done = handle(&mut st, &spec, &json!({"id":req["id"],"result":{"stopReason":"end_turn"}}), &em).unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0]["method"], "session/new");
        assert!(st.compacting.is_some() && st.prompting, "queued messages still wait");
        handle(&mut st, &spec, &json!({"id":done[0]["id"],"result":{"sessionId":"new",
            "configOptions":[{"id":"model","category":"model","currentValue":"auto"}]}}), &em).unwrap();
        assert_eq!(st.session_id.as_deref(), Some("new"));
        assert!(st.compacting.is_none() && !st.prompting && st.cursor_compact.is_none());
        // Back on the model and parameters in use
        assert_eq!(st.config_queue, [("model".to_string(), "gpt-6".to_string()), ("fast".into(), "true".into())]);
        assert!(st.system_notice.as_deref().is_some_and(|n| n.contains("be brief")));
        let pending = crate::hub::test_pending_handoff(&hub, "s").unwrap();
        assert!(pending.starts_with("<yi-compact-summary>") && pending.contains("Goal: fix x."), "{pending}");
        let evs: Vec<String> = hub.store.events_since("s", 0, 100).unwrap().into_iter().filter_map(|e| match e.body {
            EventBody::AssistantText { text, .. } => Some(format!("text:{text}")),
            EventBody::Compact { phase, trigger, .. } => Some(format!("compact:{phase}:{}", trigger.unwrap_or_default())),
            EventBody::TurnEnd { stop_reason, .. } => Some(format!("end:{}", stop_reason.unwrap_or_default())),
            _ => None,
        }).collect();
        assert_eq!(evs, ["compact:start:manual", "compact:done:manual", "end:end_turn"]);
        assert_eq!(hub.info("s").unwrap().external_id.as_deref(), Some("new"));
        // The next real message carries the summary ahead of the user's text
        let next = prompt(&mut st, &em, &Prompt { text: format!("{pending}\n\ncontinue"), images: vec![] });
        let texts: Vec<&str> = next["params"]["prompt"].as_array().unwrap().iter().map(|b| b["text"].as_str().unwrap()).collect();
        assert!(texts[0].contains("be brief") && texts[1].ends_with("continue"), "{texts:?}");
    }

    #[test]
    fn cursor_compact_failure_keeps_the_old_session() {
        let (hub, em, _rx) = crate::hub::test_emitter();
        let spec = cursor_spec("grok-4.7");
        for (result, stop) in [(json!({"result":{"stopReason":"cancelled"}}), "cancelled"), (json!({"result":{"stopReason":"end_turn"}}), "end_turn"),
            (json!({"error":{"code":-32603,"message":"Internal error"}}), "error")] {
            let mut st = State { is_cursor: true, session_id: Some("old".into()), ..Default::default() };
            let req = prompt(&mut st, &em, &Prompt { text: "/compact".into(), images: vec![] });
            let mut resp = result.clone();
            resp["id"] = req["id"].clone();
            assert!(handle(&mut st, &spec, &resp, &em).unwrap().is_empty(), "no session/new for {stop}");
            assert_eq!(st.session_id.as_deref(), Some("old"));
            assert!(st.compacting.is_none() && !st.prompting && st.cursor_compact.is_none());
            let last: Vec<_> = hub.store.events_since("s", 0, 1000).unwrap().into_iter().rev().take(2).map(|e| e.body).collect();
            assert!(matches!(&last[0], EventBody::TurnEnd { stop_reason: Some(s), .. } if s == stop), "{last:?}");
            assert!(matches!(&last[1], EventBody::Compact { phase, .. } if phase == "failed"));
        }
        assert!(crate::hub::test_pending_handoff(&hub, "s").is_none());
        // Other ACP tools still get /compact as is, and only /compact is matched
        let mut kiro = State { session_id: Some("k".into()), ..Default::default() };
        assert_eq!(prompt(&mut kiro, &em, &Prompt { text: "/compact".into(), images: vec![] })["params"]["prompt"][0]["text"], "/compact");
        assert!(is_compact_command(" /compact focus on tests") && !is_compact_command("/compactly") && !is_compact_command("hi /compact"));
    }

    #[test]
    fn cursor_auth_failure_does_not_start_a_browser_login() {
        let (_hub, em, _rx) = crate::hub::test_emitter();
        let spec = cursor_spec("grok-4.7");
        for pending in [Pending::Session, Pending::Load] {
            let mut st = State {
                auth_method: Some("cursor_login".into()),
                ..Default::default()
            };
            st.pending.insert(1, pending);
            let err = handle(&mut st, &spec, &json!({"id":1,"error":{
                "code":-32000,"message":"Authentication required"
            }}), &em).unwrap_err();
            assert!(err.contains("cursor-agent login"));
            assert!(!st.auth_tried);
            assert!(!st.pending.values().any(|p| *p == Pending::Auth));
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

        // Switching to a model without thinking levels clears stale thought_levels and never sends thought_level
        let mut st2 = State {
            is_antigravity: true,
            session_id: Some("s".into()),
            effort: Some("medium".into()),
            agy_models: models.clone(),
            ..Default::default()
        };
        read_thought_level(&mut st2, &r);
        assert_eq!(st2.agy_model.as_deref(), Some("gemini-4-argon"));
        let _ = set_model(&mut st2, "gpt-oss-120b-medium");
        assert!(st2.thought_levels.is_empty(), "stale levels from previous model cleared on set_model");
        st2.pending.clear();
        st2.agy_model = Some("gpt-oss-120b-medium".into());
        assert!(next_effort(&mut st2).is_none(), "never send effort after set_model before new configOptions");
        // Even if configOptions still advertises thought_level while on a model whose availableModels has no levels
        st2.thought_levels = vec!["low".into(), "medium".into(), "high".into()];
        st2.thought_current = Some("low".into());
        assert!(next_effort(&mut st2).is_none(), "model in agy_models has no efforts");

        // Clearing effort to default resets once without sticking st.effort across later model switches
        let mut st3 = State {
            is_antigravity: true,
            session_id: Some("s".into()),
            effort: Some("high".into()),
            agy_models: models.clone(),
            ..Default::default()
        };
        read_thought_level(&mut st3, &r);
        st3.thought_current = Some("high".into());
        st3.effort = None;
        let reset = reset_effort_to_default(&mut st3).unwrap();
        assert_eq!((reset["params"]["configId"].as_str(), reset["params"]["value"].as_str()), (Some("thought_level"), Some("medium")));
        st3.pending.clear();
        assert!(next_effort(&mut st3).is_none(), "default choice is not persisted as an override");

        // Resuming a session with a stale effort when targeting a model without levels drops st.effort
        let (hub, em, _rx) = crate::hub::test_emitter();
        crate::hub::test_set_tool(&hub, "s", ToolKind::Antigravity);
        let mut st4 = State {
            is_antigravity: true,
            effort: Some("medium".into()),
            ..Default::default()
        };
        let mut sp = StartSpec {
            tool: ToolKind::Antigravity,
            mode: SessionMode::Structured,
            cwd: std::path::PathBuf::from("."),
            resume: Some("agy-1".into()),
            session_id: None,
            replay: false,
            replay_skip: 0,
            model: Some("gpt-oss-120b-medium".into()),
            effort: Some("medium".into()),
            model_config_options: vec![],
            permission_mode: "default".into(),
            command: None,
            program: std::path::PathBuf::new(),
            extra_args: vec![],
            mcp_servers: vec![],
            mcp_fingerprint: String::new(),
            allow_mcp: vec![],
            system_notice: false,
            system_prompt: None,
            memory_prompt: None,
            memory_notice: false,
            cols: 120,
            rows: 32,
            tmux: None,
            env: std::collections::BTreeMap::new(),
            env_unset: vec![],
            data_dir: None,
            auth_method: None,
            account_handoff: None,
            fork_session: false,
            fork_fallback_handoff: None,
        };
        let req = resume_session(&mut st4, &sp, "agy-1");
        let outs = handle(&mut st4, &sp, &json!({"id": req["id"], "result": r}), &em).unwrap();
        assert_eq!(st4.effort, None, "effort cleared when target model offers no levels");
        assert!(outs.iter().any(|m| m["method"] == "session/set_model" && m["params"]["modelId"] == "gpt-oss-120b-medium"));
        let set_id = outs.iter().find(|m| m["method"] == "session/set_model").unwrap()["id"].clone();
        let _ = handle(&mut st4, &sp, &json!({"id": set_id, "result": {}}), &em).unwrap();
        assert!(next_effort(&mut st4).is_none());
        sp.model = None;
        let _ = sp;

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

    #[test]
    fn antigravity_exit_closes_background_calls_without_updating_session_time() {
        use super::*;

        let (hub, em, _rx) = crate::hub::test_emitter();
        let root = tempfile::tempdir().unwrap();
        let watch = AgyWatch::new(
            "p".into(), root.path().into(), 0, 0, None, HashMap::new(), HashMap::new(), true,
        )
        .with_open_bg_calls(VecDeque::from([("tc-bg".into(), "run_command".into())]));
        let mut st = State { agy_watch: Some(watch), ..Default::default() };

        drop_agy_watch(&mut st, &em);
        em.exited(None);

        assert_eq!(hub.info("s").unwrap().updated_at, 0);
        let events = hub.backlog("s", 0).unwrap();
        assert!(matches!(&events[0].body, EventBody::ToolResult { id, .. } if id == "tc-bg"));
    }

    pub(super) fn agy_test_spec() -> StartSpec {
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
            memory_prompt: None,
            memory_notice: false,
            cols: 80,
            rows: 24,
            tmux: None,
            session_id: None,
            env: Default::default(),
            env_unset: vec![],
            data_dir: None,
            auth_method: None,
            account_handoff: None,
            fork_session: false,
            fork_fallback_handoff: None,
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
        assert!(!is_agy_wakeup(&json!({
            "type":"SYSTEM_MESSAGE",
            "content":"The following is a <SYSTEM_MESSAGE> not actually sent by the user.\n\n<SYSTEM_MESSAGE>\n[plan review due] Call `enter_plan_mode` now.\n</SYSTEM_MESSAGE>"
        })));
        assert!(!is_agy_wakeup(&json!({
            "type":"SYSTEM_MESSAGE",
            "content":"The following is a <SYSTEM_MESSAGE> not actually sent by the user.\n\n<SYSTEM_MESSAGE>\nPlan mode is on.\n</SYSTEM_MESSAGE>"
        })));
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

    #[test]
    fn antigravity_parent_background_command_and_wakeup() {
        use super::*;
        let root = std::env::temp_dir().join(format!("yi-agy-parent-bg-{}", uuid::Uuid::new_v4()));
        let parent_id = "p-bg-1";
        let (hub, em, _rx) = crate::hub::test_emitter();
        let mut st = State {
            is_antigravity: true,
            session_id: Some(parent_id.into()),
            ..Default::default()
        };

        let bg_notice = "Created At: 2026-11-16T09:38:36+08:00\nTool is running as a background task with task id: p-bg-1/task-2\nTask Description: npm run build\nTask logs are available at: file:///tmp/task-2.log\nYOU MUST TAKE ONE OF THE FOLLOWING TWO ACTIONS: end turn";
        update(
            &mut st,
            &em,
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-cmd-1",
                "kind": "other",
                "title": "run_command",
                "rawInput": {"CommandLine": "npm run build", "toolSummary": "Frontend build"}
            }),
        );
        update(
            &mut st,
            &em,
            &json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "tc-cmd-1",
                "status": "completed",
                "rawOutput": {"output": bg_notice}
            }),
        );

        assert_eq!(st.agy_bg_tasks.len(), 1);
        assert!(!hub.store.events_since("s", 0, 100).unwrap().iter().any(|e| matches!(e.body, EventBody::ToolResult { .. })));
        let tasks = hub.info("s").unwrap().background_tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "p-bg-1/task-2");
        assert_eq!(tasks[0].kind, "local_bash");
        assert_eq!(tasks[0].description, "Frontend build");
        assert_eq!(tasks[0].output_file.as_deref(), Some("/tmp/task-2.log"));
        assert!(tasks[0].unstoppable);
        assert!(hub.stop_task("s", "p-bg-1/task-2").unwrap_err().to_string().contains("不支持从外部停止"));

        let mut steps = vec![
            json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"build it"}),
            json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{"CommandLine":"npm run build","toolSummary":"Frontend build"}}]}),
            json!({"step_index":2,"source":"MODEL","type":"GENERIC","status":"RUNNING","content":bg_notice}),
            json!({"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"Running build in background."}),
        ];
        write_agy_steps(&root, parent_id, &steps);

        let mut watch = AgyWatch::new(parent_id.into(), root.clone(), 0, 4, Some("end_turn".into()), HashMap::new(), HashMap::new(), true);
        watch.adopt_bg(std::mem::take(&mut st.agy_bg_tasks), HashSet::new());
        watch.detached = true;
        let spec = agy_test_spec();
        let (mut next_id, mut pending) = (10, HashMap::new());

        assert!(matches!(step_agy_watch(&mut watch, &spec, &em, &mut next_id, &mut pending), AgyStep::Wait));
        assert_eq!(hub.info("s").unwrap().background_tasks.len(), 1);
        assert!(!watch.reopened);

        steps.push(json!({
            "step_index": 4,
            "source": "SYSTEM",
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "content": "<SYSTEM_MESSAGE>\n[Message] timestamp=x sender=p-bg-1/task-2 priority=MESSAGE_PRIORITY_HIGH content=Task id \"p-bg-1/task-2\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nBUILD-OK\r\n\nLog: file:///tmp/task-2.log\n</SYSTEM_MESSAGE>"
        }));
        steps.push(json!({
            "step_index": 5,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "content": "Build finished: BUILD-OK"
        }));
        write_agy_steps(&root, parent_id, &steps);

        assert!(matches!(step_agy_watch(&mut watch, &spec, &em, &mut next_id, &mut pending), AgyStep::Wait));
        let AgyStep::Done(Some(sync)) = step_agy_watch(&mut watch, &spec, &em, &mut next_id, &mut pending) else {
            panic!("should emit session/resume");
        };
        assert_eq!(sync["method"], "session/resume");
        assert!(watch.reopened);
        assert!(hub.info("s").unwrap().background_tasks.is_empty());

        let evs = hub.store.events_since("s", 0, 100).unwrap();
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::ToolResult { id, output, is_error } if id == "tc-cmd-1" && output == "BUILD-OK" && !is_error)));
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::System { text } if text == "后台任务完成：Frontend build")));
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::AssistantText { text, .. } if text == "Build finished: BUILD-OK")));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn antigravity_schedule_workflow_cancel_and_daemon() {
        use super::*;
        assert_eq!(uri_to_path("file:///root/a.log"), "/root/a.log");
        assert_eq!(uri_to_path("file:///C:/Users/di3.li/a.log"), "C:/Users/di3.li/a.log");

        let root = std::env::temp_dir().join(format!("yi-agy-other-bg-{}", uuid::Uuid::new_v4()));
        let parent_id = "p-bg-2";
        let (hub, em, _rx) = crate::hub::test_emitter();
        let mut st = State {
            is_antigravity: true,
            session_id: Some(parent_id.into()),
            ..Default::default()
        };

        // 1. IsDaemon: true must NOT defer ToolResult or enter agy_bg_tasks
        update(
            &mut st,
            &em,
            &json!({"sessionUpdate": "tool_call", "toolCallId": "tc-daemon", "kind": "other", "title": "run_command", "rawInput": {"CommandLine": "npm run dev", "IsDaemon": true}}),
        );
        update(
            &mut st,
            &em,
            &json!({"sessionUpdate": "tool_call_update", "toolCallId": "tc-daemon", "status": "completed", "rawOutput": {"output": "Created At: x\nTool is running as a background task with task id: p-bg-2/task-1\nTask Description: npm run dev"}}),
        );
        assert!(st.agy_bg_tasks.is_empty());
        assert!(hub.store.events_since("s", 0, 100).unwrap().iter().any(|e| matches!(&e.body, EventBody::ToolResult { id, .. } if id == "tc-daemon")));

        // 2. Workflow + Timer with TimerCondition + Canceled command in transcript
        let wf_notice = "Created At: x\nTool is running as a background task with task id: p-bg-2/task-2\nTask Description: Review files\nTask logs are available at: file:///tmp/task-2.log";
        let tm_notice = "Created At: x\nTool is running as a background task with task id: p-bg-2/task-3\nTask Description: Timer: 30s, Prompt: Check workflow\nTask logs are available at: file:///tmp/task-3.log";
        let cmd_notice = "Created At: x\nTool is running as a background task with task id: p-bg-2/task-4\nTask Description: sleep 60\nTask logs are available at: file:///tmp/task-4.log";
        let mut steps = vec![
            json!({"step_index":0,"type":"USER_INPUT","status":"DONE","content":"go"}),
            json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[
                {"name":"run_workflow","args":{"Description":"Review workflow"}},
                {"name":"schedule","args":{"DurationSeconds":"30","Prompt":"\"Check workflow\"","TimerCondition":"\"p-bg-2/task-2\""}},
                {"name":"run_command","args":{"CommandLine":"sleep 60"}}
            ]}),
            json!({"step_index":2,"type":"GENERIC","status":"RUNNING","content":wf_notice}),
            json!({"step_index":3,"type":"GENERIC","status":"RUNNING","content":tm_notice}),
            json!({"step_index":4,"type":"GENERIC","status":"RUNNING","content":cmd_notice}),
            json!({"step_index":5,"type":"PLANNER_RESPONSE","status":"DONE","content":"Running."}),
        ];
        write_agy_steps(&root, parent_id, &steps);
        assert!(has_pending_agy_async_turn(&root, parent_id, 0));

        let mut watch = AgyWatch::new(parent_id.into(), root.clone(), 0, 6, Some("end_turn".into()), HashMap::new(), HashMap::new(), true);
        watch.detached = true;
        let spec = agy_test_spec();
        let (mut next_id, mut pending) = (1, HashMap::new());

        assert!(matches!(step_agy_watch(&mut watch, &spec, &em, &mut next_id, &mut pending), AgyStep::Wait));
        let tasks = hub.info("s").unwrap().background_tasks;
        assert_eq!(tasks.len(), 3);
        assert_eq!((tasks[0].kind.as_str(), tasks[0].description.as_str()), ("workflow", "Review workflow"));
        assert_eq!((tasks[1].kind.as_str(), tasks[2].kind.as_str()), ("timer", "local_bash"));

        // Workflow completes -> TimerCondition automatically cancels task-3! Task-4 is cancelled explicitly.
        steps.push(json!({"step_index":6,"type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=x sender=p-bg-2/task-2 priority=MESSAGE_PRIORITY_HIGH content=Task id \"p-bg-2/task-2\" finished with result:\nWorkflow done\nLog: file:///tmp/task-2.log\n</SYSTEM_MESSAGE>"}));
        steps.push(json!({"step_index":7,"type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=x sender=p-bg-2/task-4 priority=MESSAGE_PRIORITY_LOW content=Task id \"p-bg-2/task-4\" was canceled with result:\nTool execution was canceled\nLog: file:///tmp/task-4.log\n</SYSTEM_MESSAGE>"}));
        steps.push(json!({"step_index":8,"type":"PLANNER_RESPONSE","status":"DONE","content":"All settled."}));
        write_agy_steps(&root, parent_id, &steps);

        assert!(matches!(step_agy_watch(&mut watch, &spec, &em, &mut next_id, &mut pending), AgyStep::Wait));
        assert!(matches!(step_agy_watch(&mut watch, &spec, &em, &mut next_id, &mut pending), AgyStep::Done(Some(_))));
        assert!(hub.info("s").unwrap().background_tasks.is_empty());

        let evs = hub.store.events_since("s", 0, 100).unwrap();
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::System { text } if text == "后台任务完成：Review workflow")));
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::System { text } if text == "后台任务已停止：sleep 60")));
        assert!(!evs.iter().any(|e| matches!(&e.body, EventBody::System { text } if text.contains("Timer: 30s"))));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn antigravity_in_progress_background_command_claimed_from_transcript() {
        use super::*;
        let root = std::env::temp_dir().join(format!("yi-agy-inprog-{}", uuid::Uuid::new_v4()));
        let parent_id = "p-bg-3";
        let (hub, em, _rx) = crate::hub::test_emitter();
        let mut st = State {
            is_antigravity: true,
            session_id: Some(parent_id.into()),
            ..Default::default()
        };

        // Real agy acp 1.5.9 leaves run_command in_progress over ACP when WaitMsBeforeAsync expires
        update(
            &mut st,
            &em,
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-live-1",
                "name": "run_command",
                "title": "ssh win build",
                "status": "in_progress",
                "rawInput": {"CommandLine": "ssh win build", "toolSummary": "Sync and build"}
            }),
        );
        assert_eq!(st.agy_open_bg_calls.len(), 1);

        let bg_notice = "Created At: x\nTool is running as a background task with task id: p-bg-3/task-1\nTask Description: ssh win build\nTask logs are available at: file:///tmp/task-1.log";
        let mut steps = vec![
            json!({"step_index":0,"type":"USER_INPUT","status":"DONE","content":"build"}),
            json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{"CommandLine":"ssh win build","toolSummary":"Sync and build"}}]}),
            json!({"step_index":2,"type":"GENERIC","status":"RUNNING","content":bg_notice}),
            json!({"step_index":3,"type":"PLANNER_RESPONSE","status":"DONE","content":"Waiting."}),
        ];
        write_agy_steps(&root, parent_id, &steps);

        assert!(discover_agy_bg_from_steps(&mut st, &steps));
        assert!(st.agy_open_bg_calls.is_empty());
        assert_eq!(st.agy_bg_tasks.len(), 1);
        assert_eq!(st.agy_bg_tasks[0].tc_id.as_deref(), Some("tc-live-1"));

        // Late empty failed update from agy acp when canceled must defer to SYSTEM_MESSAGE
        update(
            &mut st,
            &em,
            &json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "tc-live-1",
                "status": "failed"
            }),
        );
        assert!(!hub.store.events_since("s", 0, 100).unwrap().iter().any(|e| matches!(e.body, EventBody::ToolResult { .. })));

        steps.push(json!({"step_index":4,"type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=x sender=p-bg-3/task-1 priority=MESSAGE_PRIORITY_LOW content=Task id \"p-bg-3/task-1\" was canceled with result:\nTool execution was canceled\nLog: file:///tmp/task-1.log\n</SYSTEM_MESSAGE>"}));
        write_agy_steps(&root, parent_id, &steps);
        settle_agy_bg_from_steps(&mut st, &steps, &em);

        let evs = hub.store.events_since("s", 0, 100).unwrap();
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::ToolResult { id, output, is_error } if id == "tc-live-1" && output == "Tool execution was canceled" && *is_error)));
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::System { text } if text == "后台任务已停止：Sync and build")));

        // Even after bg_tasks move into st.agy_watch on TurnEnd, a late empty `{}` completed update must defer to SYSTEM_MESSAGE
        update(
            &mut st,
            &em,
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-live-2",
                "name": "run_command",
                "title": "echo ok",
                "status": "in_progress",
                "rawInput": {"CommandLine": "echo ok", "toolSummary": "Watch defer test"}
            }),
        );
        let bg2 = "Created At: x\nTool is running as a background task with task id: p-bg-3/task-2\nTask Description: echo ok\nTask logs are available at: file:///tmp/task-2.log";
        steps.push(json!({"step_index":5,"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{"CommandLine":"echo ok","toolSummary":"Watch defer test"}}]}));
        steps.push(json!({"step_index":6,"type":"GENERIC","status":"RUNNING","content":bg2}));
        steps.push(json!({"step_index":7,"type":"PLANNER_RESPONSE","status":"DONE","content":"Waiting 2."}));
        write_agy_steps(&root, parent_id, &steps);
        assert!(discover_agy_bg_from_steps(&mut st, &steps));
        let mut watch = AgyWatch::new(parent_id.into(), root.clone(), 0, 8, Some("end_turn".into()), HashMap::new(), HashMap::new(), true);
        watch.adopt_bg(std::mem::take(&mut st.agy_bg_tasks), HashSet::new());
        watch.detached = true;
        st.agy_watch = Some(watch);

        update(
            &mut st,
            &em,
            &json!({"sessionUpdate": "tool_call_update", "toolCallId": "tc-live-2", "status": "completed", "rawOutput": {}}),
        );
        assert!(!hub.store.events_since("s", 0, 100).unwrap().iter().any(|e| matches!(&e.body, EventBody::ToolResult { id, .. } if id == "tc-live-2")));

        steps.push(json!({"step_index":8,"type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=x sender=p-bg-3/task-2 priority=MESSAGE_PRIORITY_HIGH content=Task id \"p-bg-3/task-2\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nWATCH_DEFER_OK\nLog: file:///tmp/task-2.log\n</SYSTEM_MESSAGE>"}));
        write_agy_steps(&root, parent_id, &steps);
        let _ = poll_agy_watch(&mut st, &agy_test_spec(), &em);
        let evs2 = hub.store.events_since("s", 0, 100).unwrap();
        assert!(evs2.iter().any(|e| matches!(&e.body, EventBody::ToolResult { id, output, is_error } if id == "tc-live-2" && output == "WATCH_DEFER_OK" && !*is_error)));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn antigravity_active_wakeup_survives_past_wakeup_grace() {
        use super::*;
        let root = std::env::temp_dir().join(format!("yi-agy-grace-{}", uuid::Uuid::new_v4()));
        let parent_id = "p-wake-grace";
        let (hub, em, _rx) = crate::hub::test_emitter();
        let bg1 = "Created At: x\nTool is running as a background task with task id: p-wake-grace/task-1\nTask Description: build\nTask logs are available at: file:///tmp/t1.log";
        let mut steps = vec![
            json!({"step_index":0,"type":"USER_INPUT","status":"DONE","content":"go"}),
            json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{"CommandLine":"sleep 5","toolSummary":"Step 1"}}]}),
            json!({"step_index":2,"type":"GENERIC","status":"RUNNING","content":bg1}),
            json!({"step_index":3,"type":"PLANNER_RESPONSE","status":"DONE","content":"Running step 1."}),
            // Task 1 finished and woke the parent; the parent ran read_file and now a slow command is still in flight
            json!({"step_index":4,"type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=x sender=p-wake-grace/task-1 priority=MESSAGE_PRIORITY_HIGH content=Task id \"p-wake-grace/task-1\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nok-1\nLog: file:///tmp/t1.log\n</SYSTEM_MESSAGE>"}),
            json!({"step_index":5,"type":"PLANNER_RESPONSE","status":"DONE","thinking":"Inspecting files","tool_calls":[{"name":"read_file","args":{"Path":"Cargo.toml"}}]}),
            json!({"step_index":6,"type":"GENERIC","status":"DONE","content":"[workspace]"}),
            json!({"step_index":7,"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{"CommandLine":"cargo test","toolSummary":"Step 2 test"}}]}),
            json!({"step_index":8,"type":"GENERIC","status":"RUNNING","content":"waiting 10s..."}),
        ];
        write_agy_steps(&root, parent_id, &steps);

        let mut st = State { is_antigravity: true, session_id: Some(parent_id.into()), ..Default::default() };
        let mut watch = AgyWatch::new(parent_id.into(), root.clone(), 0, 4, Some("end_turn".into()), HashMap::new(), HashMap::new(), true);
        watch.detached = true;
        watch.add_bg_task(AgyBgTask {
            id: "p-wake-grace/task-1".into(),
            kind: "local_bash".into(),
            description: "Step 1".into(),
            started_at: 1,
            output_file: None,
            tc_id: Some("tc-1".into()),
            is_daemon: false,
            timer_cond: None,
            done: false,
            announced: false,
            result: None,
        });
        st.agy_watch = Some(watch);

        assert!(poll_agy_watch(&mut st, &agy_test_spec(), &em).is_none());
        // Simulate WAKEUP_GRACE (20s) elapsing while the wakeup turn is actively running step 8
        st.agy_watch.as_mut().unwrap().all_subagents_done_at = Some(tokio::time::Instant::now() - std::time::Duration::from_secs(25));
        assert!(poll_agy_watch(&mut st, &agy_test_spec(), &em).is_none());
        assert!(st.agy_watch.is_some(), "active wakeup turn must not be aborted by WAKEUP_GRACE");

        // Now step 8 spills into background task-2 and the intermediate reply settles
        steps[8] = json!({"step_index":8,"type":"GENERIC","status":"RUNNING","content":"Created At: x\nTool is running as a background task with task id: p-wake-grace/task-2\nTask Description: cargo test\nTask logs are available at: file:///tmp/t2.log"});
        steps.push(json!({"step_index":9,"type":"PLANNER_RESPONSE","status":"DONE","content":"Launched step 2 test in background."}));
        write_agy_steps(&root, parent_id, &steps);

        assert!(poll_agy_watch(&mut st, &agy_test_spec(), &em).is_none());
        assert!(poll_agy_watch(&mut st, &agy_test_spec(), &em).is_none());
        let tasks = hub.info("s").unwrap().background_tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "p-wake-grace/task-2");
        assert!(!st.agy_watch.as_ref().unwrap().reopened);

        // Task 2 finishes and writes final response
        steps.push(json!({"step_index":10,"type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=x sender=p-wake-grace/task-2 priority=MESSAGE_PRIORITY_HIGH content=Task id \"p-wake-grace/task-2\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nALL PASSED\nLog: file:///tmp/t2.log\n</SYSTEM_MESSAGE>"}));
        steps.push(json!({"step_index":11,"type":"PLANNER_RESPONSE","status":"DONE","content":"Final result: all passed."}));
        write_agy_steps(&root, parent_id, &steps);

        assert!(poll_agy_watch(&mut st, &agy_test_spec(), &em).is_none());
        let sync = poll_agy_watch(&mut st, &agy_test_spec(), &em).expect("final wakeup should send session/resume");
        assert_eq!(sync["method"], "session/resume");
        assert!(hub.info("s").unwrap().background_tasks.is_empty());

        let evs = hub.store.events_since("s", 0, 100).unwrap();
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::AssistantText { text, .. } if text == "Launched step 2 test in background.")));
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::ToolResult { id, output, is_error } if id == "agy-wake:p-wake-grace:7:0" && output == "ALL PASSED" && !*is_error)));
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::AssistantText { text, .. } if text == "Final result: all passed.")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn antigravity_late_acp_reply_rearms_watch_and_is_saved_once() {
        use super::*;
        let root = tempfile::tempdir().unwrap();
        let parent_id = "p-late-reply";
        let (hub, em, _rx) = crate::hub::test_emitter();
        let mut steps = vec![
            json!({"type":"USER_INPUT","status":"DONE","content":"build"}),
            json!({"type":"PLANNER_RESPONSE","status":"DONE","content":"Waiting for the build."}),
            json!({"type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=x sender=p-late-reply/task-1 priority=MESSAGE_PRIORITY_HIGH content=Build finished\n</SYSTEM_MESSAGE>"}),
        ];
        write_agy_steps(root.path(), parent_id, &steps);
        let mut st = State {
            is_antigravity: true,
            session_id: Some(parent_id.into()),
            agy_wake_from: agy_prompt_boundary(&steps, 0, "Waiting for the build."),
            ..Default::default()
        };
        assert_eq!(st.agy_wake_from, 2, "the wakeup already on disk must be watched");
        em.event(EventBody::TurnEnd { stop_reason: Some("end_turn".into()), model: None });

        let mut watch = AgyWatch::new(
            parent_id.into(), root.path().to_path_buf(), 2, 2, None, HashMap::new(), HashMap::new(), true,
        );
        watch.detached = true;
        st.agy_watch = Some(watch);
        let spec = agy_test_spec();
        assert!(poll_agy_watch(&mut st, &spec, &em).is_none());
        st.agy_watch.as_mut().unwrap().all_subagents_done_at =
            Some(tokio::time::Instant::now() - std::time::Duration::from_secs(25));
        assert!(poll_agy_watch(&mut st, &spec, &em).is_none());
        assert!(st.agy_watch.is_some(), "a known wakeup may take longer than 20s to respond");
        assert_eq!(hub.info("s").unwrap().status, SessionStatus::Idle);

        // The ACP update also has to recover if no watch is available. Child updates must not do it.
        st.agy_watch = None;
        let mut chunk = json!({
            "sessionId":"a-child",
            "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Build passed"}}
        });
        assert!(agy_detached_update(&mut st, &chunk, || root.path().to_path_buf()));
        assert!(st.agy_watch.is_none());
        chunk["sessionId"] = json!(parent_id);
        assert!(agy_detached_update(&mut st, &chunk, || root.path().to_path_buf()));
        assert!(poll_agy_watch(&mut st, &spec, &em).is_none());
        assert_eq!(hub.info("s").unwrap().status, SessionStatus::Running);

        steps.push(json!({"step_index":3,"type":"PLANNER_RESPONSE","status":"RUNNING","content":"Build passed"}));
        write_agy_steps(root.path(), parent_id, &steps);
        assert!(poll_agy_watch(&mut st, &spec, &em).is_none());
        assert_eq!(st.agy_watch.as_ref().unwrap().preview, Some((3, "Build passed".into())));
        steps[3]["status"] = json!("DONE");
        steps[3]["content"] = json!("Build passed, all checks green.");
        write_agy_steps(root.path(), parent_id, &steps);
        assert!(poll_agy_watch(&mut st, &spec, &em).is_none());
        assert_eq!(poll_agy_watch(&mut st, &spec, &em).unwrap()["method"], "session/resume");
        assert_eq!(hub.info("s").unwrap().status, SessionStatus::Idle);
        assert_eq!(st.agy_wake_from, 4);

        assert!(agy_detached_update(&mut st, &chunk, || root.path().to_path_buf()));
        assert!(poll_agy_watch(&mut st, &spec, &em).is_none());
        assert_eq!(hub.info("s").unwrap().status, SessionStatus::Idle);
        let events = hub.store.events_since("s", 0, 100).unwrap();
        let replies: Vec<_> = events.iter().filter_map(|event| match &event.body {
            EventBody::AssistantText { id, text } => Some((id.as_str(), text.as_str())),
            _ => None,
        }).collect();
        assert_eq!(replies, [("agy-wake-p-late-reply-a-3", "Build passed, all checks green.")]);
        assert_eq!(events.iter().filter(|event| matches!(event.body, EventBody::TurnEnd { .. })).count(), 2);
    }

    #[test]
    fn antigravity_wakeup_streams_while_other_task_keeps_running() {
        use super::*;
        let root = std::env::temp_dir().join(format!("yi-agy-partial-{}", uuid::Uuid::new_v4()));
        let parent_id = "p-partial";
        let (hub, em, _rx) = crate::hub::test_emitter();
        let bg_a = "Created At: x\nTool is running as a background task with task id: p-partial/task-a\nTask logs are available at: file:///tmp/ta.log";
        let bg_b = "Created At: x\nTool is running as a background task with task id: p-partial/task-b\nTask logs are available at: file:///tmp/tb.log";
        let mut steps = vec![
            json!({"step_index":0,"type":"USER_INPUT","status":"DONE","content":"run two"}),
            json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{"CommandLine":"a","toolSummary":"Task A"}},{"name":"run_command","args":{"CommandLine":"b","toolSummary":"Task B"}}]}),
            json!({"step_index":2,"type":"GENERIC","status":"RUNNING","content":bg_a}),
            json!({"step_index":3,"type":"GENERIC","status":"RUNNING","content":bg_b}),
            json!({"step_index":4,"type":"PLANNER_RESPONSE","status":"DONE","content":"Both started."}),
            // Only task-a finishes; task-b keeps running. Wakeup launches task-c and replies
            json!({"step_index":5,"type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=x sender=p-partial/task-a priority=MESSAGE_PRIORITY_HIGH content=Task id \"p-partial/task-a\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nA-FAIL\nLog: file:///tmp/ta.log\n</SYSTEM_MESSAGE>"}),
            json!({"step_index":6,"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{"CommandLine":"c","toolSummary":"Task C"}}]}),
            json!({"step_index":7,"type":"GENERIC","status":"RUNNING","content":"Created At: x\nTool is running as a background task with task id: p-partial/task-c\nTask logs are available at: file:///tmp/tc.log"}),
            json!({"step_index":8,"type":"PLANNER_RESPONSE","status":"DONE","content":"Task A done; Task C started while Task B runs."}),
        ];
        write_agy_steps(&root, parent_id, &steps);

        let mut st = State { is_antigravity: true, session_id: Some(parent_id.into()), ..Default::default() };
        let mut watch = AgyWatch::new(parent_id.into(), root.clone(), 0, 5, Some("end_turn".into()), HashMap::new(), HashMap::new(), true);
        watch.detached = true;
        watch.add_bg_task(AgyBgTask { id: "p-partial/task-a".into(), kind: "local_bash".into(), description: "Task A".into(), started_at: 1, output_file: None, tc_id: Some("tc-a".into()), is_daemon: false, timer_cond: None, done: false, announced: false, result: None });
        watch.add_bg_task(AgyBgTask { id: "p-partial/task-b".into(), kind: "local_bash".into(), description: "Task B".into(), started_at: 2, output_file: None, tc_id: Some("tc-b".into()), is_daemon: false, timer_cond: None, done: false, announced: false, result: None });
        st.agy_watch = Some(watch);

        // Two ticks settle the intermediate CycleEnd without blocking on task-b
        assert!(poll_agy_watch(&mut st, &agy_test_spec(), &em).is_none());
        assert!(poll_agy_watch(&mut st, &agy_test_spec(), &em).is_none());
        let tasks: Vec<String> = hub.info("s").unwrap().background_tasks.into_iter().map(|t| t.id).collect();
        assert_eq!(tasks, vec!["p-partial/task-b", "p-partial/task-c"]);
        assert!(!st.agy_watch.as_ref().unwrap().reopened);

        let mid_evs = hub.store.events_since("s", 0, 100).unwrap();
        assert!(mid_evs.iter().any(|e| matches!(&e.body, EventBody::AssistantText { text, .. } if text == "Task A done; Task C started while Task B runs.")));
        assert!(mid_evs.iter().any(|e| matches!(&e.body, EventBody::TurnEnd { .. })));

        // Task B & C finish -> final reply & session/resume
        steps.push(json!({"step_index":9,"type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=x sender=p-partial/task-b priority=MESSAGE_PRIORITY_HIGH content=Task id \"p-partial/task-b\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nB-OK\nLog: file:///tmp/tb.log\n</SYSTEM_MESSAGE>"}));
        steps.push(json!({"step_index":10,"type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=x sender=p-partial/task-c priority=MESSAGE_PRIORITY_HIGH content=Task id \"p-partial/task-c\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nC-OK\nLog: file:///tmp/tc.log\n</SYSTEM_MESSAGE>"}));
        steps.push(json!({"step_index":11,"type":"PLANNER_RESPONSE","status":"DONE","content":"All three complete."}));
        write_agy_steps(&root, parent_id, &steps);

        assert!(poll_agy_watch(&mut st, &agy_test_spec(), &em).is_none());
        let sync = poll_agy_watch(&mut st, &agy_test_spec(), &em).expect("final session/resume");
        assert_eq!(sync["method"], "session/resume");
        let end_evs = hub.store.events_since("s", 0, 100).unwrap();
        assert!(end_evs.iter().any(|e| matches!(&e.body, EventBody::ToolResult { id, output, .. } if id == "agy-wake:p-partial:6:0" && output == "C-OK")));
        assert!(end_evs.iter().any(|e| matches!(&e.body, EventBody::AssistantText { text, .. } if text == "All three complete.")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn antigravity_in_progress_bg_call_handed_to_watch_and_deduped_replay() {
        use super::*;
        // 1. Muted session/load suppresses repeated tool_call ids if emitted later
        let (_hub, em, _rx) = crate::hub::test_emitter();
        let mut st = State { is_antigravity: true, loading: true, ..Default::default() };
        update(&mut st, &em, &json!({"sessionUpdate":"tool_call","toolCallId":"tc-muted-1","name":"read_file","status":"completed"}));
        st.loading = false;
        update(&mut st, &em, &json!({"sessionUpdate":"tool_call","toolCallId":"tc-muted-1","name":"read_file","status":"in_progress"}));
        assert!(!st.tools.contains_key("tc-muted-1"));

        // 2. Carry + unresumed wakeup prefix suppresses duplicate replay in the next prompt while passing new text
        let (hub2, em2, _rx2) = crate::hub::test_emitter();
        let mut st2 = State { is_antigravity: true, session_id: Some("p-dedup".into()), ..Default::default() };
        st2.agy_carry.unresumed_items.push_back(AgyWakeReplayItem::Thinking("Wake thought".into()));
        st2.agy_carry.unresumed_items.push_back(AgyWakeReplayItem::Tool { name: "read_file".into(), input: json!({"Path":"a.rs"}) });
        st2.agy_carry.unresumed_items.push_back(AgyWakeReplayItem::Assistant("Part 1 done".into()));
        let _ = prompt(&mut st2, &em2, &Prompt { text: "next".into(), images: vec![] });

        // Replayed prefix is dropped silently (including the matched ToolCall's ToolResult)
        update(&mut st2, &em2, &json!({"sessionUpdate":"agent_thought_chunk","content":{"text":"Wake thought"}}));
        update(&mut st2, &em2, &json!({"sessionUpdate":"tool_call","toolCallId":"tc-rep-1","name":"read_file","status":"in_progress","rawInput":{"Path":"a.rs"}}));
        update(&mut st2, &em2, &json!({"sessionUpdate":"tool_call_update","toolCallId":"tc-rep-1","status":"completed","rawOutput":{"output":"file body"}}));
        update(&mut st2, &em2, &json!({"sessionUpdate":"agent_message_chunk","content":{"text":"Part 1 done"}}));
        // Real agy emits user_message_chunk before starting the live turn
        update(&mut st2, &em2, &json!({"sessionUpdate":"user_message_chunk","content":{"text":"next"}}));
        // Now real live turn begins
        update(&mut st2, &em2, &json!({"sessionUpdate":"agent_message_chunk","content":{"text":"Part 2 fresh"}}));
        flush(&mut st2, &em2);

        let evs = hub2.store.events_since("s", 0, 100).unwrap();
        assert!(!evs.iter().any(|e| matches!(&e.body, EventBody::Thinking { text, .. } if text == "Wake thought")));
        assert!(!evs.iter().any(|e| matches!(&e.body, EventBody::ToolCall { id, .. } if id == "tc-rep-1")));
        assert!(!evs.iter().any(|e| matches!(&e.body, EventBody::ToolResult { id, .. } if id == "tc-rep-1")));
        assert!(!evs.iter().any(|e| matches!(&e.body, EventBody::AssistantText { text, .. } if text == "Part 1 done")));
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::AssistantText { text, .. } if text == "Part 2 fresh")));

        // 3. take_agy_open_bg_call handoff works across open_bg_calls
        let mut open = VecDeque::from([("tc-bg-lag".to_string(), "run_command".to_string())]);
        let mut watch = AgyWatch::new("p".into(), std::env::temp_dir(), 0, 0, None, HashMap::new(), HashMap::new(), true)
            .with_open_bg_calls(open.clone());
        assert_eq!(take_agy_open_bg_call(&mut open, None, Some("run_command"), None).as_deref(), Some("tc-bg-lag"));
        assert_eq!(take_agy_open_bg_call(&mut watch.open_bg_calls, None, Some("run_command"), None).as_deref(), Some("tc-bg-lag"));
    }

    #[test]
    fn antigravity_elicitation_ask_question_and_answers() {
        use super::*;
        assert_eq!(client_capabilities(ToolKind::Antigravity)["elicitation"]["form"], json!({}));
        assert!(client_capabilities(ToolKind::Cursor).get("elicitation").is_none());
        assert!(client_capabilities(ToolKind::Kiro).get("elicitation").is_none());

        let (hub, em, _rx) = crate::hub::test_emitter();
        let spec = agy_test_spec();
        let mut st = State { is_antigravity: true, session_id: Some("agy-s1".into()), ..Default::default() };

        // 1. Real 4-question Antigravity ask_question tool_call + elicitation/create (plus multi-select & free-text Other)
        let raw_input = json!({
            "questions": [
                {
                    "is_multi_select": false,
                    "options": [
                        "(Recommended) 基于本地 kong-ref:latest 分别挂载/覆盖 feature-2026.1-release 与当前 feature-2026.3-release 的 kong/ 代码及配置进行 A/B 对比",
                        "尝试基于本地 gw-builder:latest 或 Dockerfile 重新构建两个版本的完整镜像",
                        "使用其他指定镜像（请在 Other 中说明镜像名）"
                    ],
                    "question": "1. Kong 容器镜像如何准备（用于基线版 2026.1 vs 优化版 2026.3 对比）？"
                },
                {
                    "is_multi_select": true,
                    "options": ["PG", "Redis", "Kafka"],
                    "question": "2. 依赖中间件的容器部署范围？"
                }
            ]
        });
        update(
            &mut st,
            &em,
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_kong_1",
                "name": "ask_question",
                "kind": "other",
                "title": "Run ask_question",
                "status": "pending",
                "rawInput": raw_input,
            }),
        );
        let elicit_req = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "elicitation/create",
            "params": {
                "sessionId": "agy-s1",
                "toolCallId": "call_kong_1",
                "mode": "form",
                "message": "The agent has 2 questions.",
                "requestedSchema": {
                    "type": "object",
                    "properties": {
                        "q1": {
                            "type": "string",
                            "title": "1. Kong 容器镜像如何准备（用于基线版 2026.1 vs 优化版 2026.3 对比）？",
                            "oneOf": [
                                {"const": "1", "title": "(Recommended) 基于本地 kong-ref:latest 分别挂载/覆盖 feature-2026.1-release 与当前 feature-2026.3-release 的 kong/ 代码及配置进行 A/B 对比"},
                                {"const": "2", "title": "尝试基于本地 gw-builder:latest 或 Dockerfile 重新构建两个版本的完整镜像"},
                                {"const": "3", "title": "使用其他指定镜像（请在 Other 中说明镜像名）"}
                            ]
                        },
                        "q1_other": {
                            "type": "string",
                            "title": "Other",
                            "description": "Optional free-form answer to: 1. Kong 容器镜像如何准备（用于基线版 2026.1 vs 优化版 2026.3 对比）？"
                        },
                        "q2": {
                            "type": "array",
                            "title": "2. 依赖中间件的容器部署范围？",
                            "items": {
                                "anyOf": [
                                    {"const": "1", "title": "PG"},
                                    {"const": "2", "title": "Redis"},
                                    {"const": "3", "title": "Kafka"}
                                ]
                            }
                        },
                        "q2_other": {
                            "type": "string",
                            "title": "Other",
                            "description": "Optional free-form answer to: 2. 依赖中间件的容器部署范围？"
                        }
                    }
                }
            }
        });
        let outs = handle(&mut st, &spec, &elicit_req, &em).unwrap();
        assert!(outs.is_empty());
        assert_eq!(st.elicits.len(), 1);

        let evs = hub.store.events_since("s", 0, 50).unwrap();
        let (rid, title, qs) = evs
            .iter()
            .find_map(|e| match &e.body {
                EventBody::PermissionRequest { request_id, tool_name, title, questions, .. } if tool_name == "ask_question" => {
                    Some((request_id.clone(), title.clone(), questions.clone()))
                }
                _ => None,
            })
            .expect("PermissionRequest emitted for elicitation/create");
        assert_eq!(title, "Antigravity 有问题想问你");
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].id, "q1");
        assert!(!qs[0].multi_select);
        assert!(qs[0].allow_other);
        assert_eq!(qs[0].options.len(), 3);
        assert_eq!(qs[1].id, "q2");
        assert!(qs[1].multi_select);
        assert!(qs[1].allow_other);
        assert_eq!(qs[1].options.len(), 3);

        // Answer q1 with free-text Other, and q2 with multi-select + extra Other note
        let el = st.elicits.remove(&rid).unwrap();
        let resp = elicit_response(
            &el,
            QUESTION_ANSWER,
            &[
                QuestionAnswer { question_id: "q1".into(), selected: vec![], text: "kong:3.6-custom".into() },
                QuestionAnswer { question_id: "q2".into(), selected: vec!["1".into(), "2".into()], text: "plus mock".into() },
            ],
        );
        assert_eq!(resp["id"], "1");
        assert_eq!(resp["result"]["action"], "accept");
        assert_eq!(resp["result"]["content"]["q1_other"], "kong:3.6-custom");
        assert!(resp["result"]["content"].get("q1").is_none());
        assert_eq!(resp["result"]["content"]["q2"], json!(["1", "2"]));
        assert_eq!(resp["result"]["content"]["q2_other"], "plus mock");

        // Skip -> decline; interrupt -> cancel
        assert_eq!(elicit_response(&el, QUESTION_SKIP, &[])["result"], json!({"action": "decline"}));
        assert_eq!(elicit_response(&el, "cancelled", &[])["result"], json!({"action": "cancel"}));

        // 2. Numeric ordering with >=10 questions (q1..q10 must not put q10 before q2) + 0-option qN_other
        let mut props = serde_json::Map::new();
        for i in 1..=10 {
            props.insert(format!("q{i}_other"), json!({"type": "string", "title": format!("Q{i}")}));
        }
        let (qs10, _) = parse_elicitation(&json!({"mode": "form", "requestedSchema": {"properties": props}}), None).unwrap();
        let ids10: Vec<&str> = qs10.iter().map(|q| q.question.as_str()).collect();
        assert_eq!(ids10, ["Q1", "Q2", "Q3", "Q4", "Q5", "Q6", "Q7", "Q8", "Q9", "Q10"]);

        // 3. Non-form mode declines immediately
        let url_elicit = json!({"jsonrpc": "2.0", "id": "9", "method": "elicitation/create", "params": {"mode": "url", "url": "https://example.com"}});
        let url_out = handle(&mut st, &spec, &url_elicit, &em).unwrap();
        assert_eq!(url_out, vec![json!({"jsonrpc": "2.0", "id": "9", "result": {"action": "decline"}})]);
    }

    #[test]
    fn antigravity_prefers_session_resume_and_falls_back_to_load() {
        use super::*;
        let (hub, em, _rx) = crate::hub::test_emitter();
        let mut spec = agy_test_spec();
        spec.resume = Some("agy-existing-1".into());
        spec.replay = false;

        // 1. When initialize advertises sessionCapabilities.resume and replay=false, use session/resume
        let mut st = State { is_antigravity: true, native_modes: true, ..Default::default() };
        let init_id = st.req(Pending::Init, "initialize", json!({}))["id"].as_u64().unwrap();
        let outs = handle(
            &mut st,
            &spec,
            &json!({"jsonrpc":"2.0","id":init_id,"result":{"agentCapabilities":{"sessionCapabilities":{"resume":{},"load":{}}}}}),
            &em,
        )
        .unwrap();
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0]["method"], "session/resume");
        assert_eq!(outs[0]["params"]["sessionId"], "agy-existing-1");
        assert!(st.loading);
        assert!(st.resumed);

        // 2. Non-auth error on session/resume falls back cleanly to session/load
        let resume_id = outs[0]["id"].as_u64().unwrap();
        let fallback = handle(
            &mut st,
            &spec,
            &json!({"jsonrpc":"2.0","id":resume_id,"error":{"code":-32603,"message":"Failed to read session agy-existing-1; retry, or load it with session/load"}}),
            &em,
        )
        .unwrap();
        assert_eq!(fallback.len(), 1);
        assert_eq!(fallback[0]["method"], "session/load");
        let load_id = fallback[0]["id"].as_u64().unwrap();
        let _ = handle(
            &mut st,
            &spec,
            &json!({"jsonrpc":"2.0","id":load_id,"result":{"sessionId":"agy-existing-1"}}),
            &em,
        )
        .unwrap();
        assert!(!st.loading);
        assert_eq!(st.session_id.as_deref(), Some("agy-existing-1"));
        assert_eq!(st.agy_last_streamed_steps, 0);
        assert_eq!(hub.info("s").unwrap().status, SessionStatus::Idle);

        // 3. Explicit history import (replay=true) still uses session/load even when session/resume is advertised
        let mut import_spec = spec.clone();
        import_spec.replay = true;
        let mut st_imp = State { is_antigravity: true, native_modes: true, ..Default::default() };
        let imp_init = st_imp.req(Pending::Init, "initialize", json!({}))["id"].as_u64().unwrap();
        let imp_outs = handle(
            &mut st_imp,
            &import_spec,
            &json!({"jsonrpc":"2.0","id":imp_init,"result":{"agentCapabilities":{"sessionCapabilities":{"resume":{},"load":{}}}}}),
            &em,
        )
        .unwrap();
        assert_eq!(imp_outs[0]["method"], "session/load");

        // 4. Auth error on session/resume is surfaced instead of falling back
        let mut st_auth = State { is_antigravity: true, can_resume: true, ..Default::default() };
        let req = new_or_load(&mut st_auth, &spec);
        assert_eq!(req["method"], "session/resume");
        let rid = req["id"].as_u64().unwrap();
        let err = handle(
            &mut st_auth,
            &spec,
            &json!({"jsonrpc":"2.0","id":rid,"error":{"code":-32000,"message":"Authentication required"}}),
            &em,
        )
        .unwrap_err();
        assert!(err.contains("agy acp --login"), "{err}");
    }

    #[test]
    fn antigravity_reopen_replay_guard_filters_history_concat_quota_and_preserves_new_reply() {
        use super::*;
        let root = tempfile::tempdir().unwrap();
        let sid = "agy-reopen-replay";
        let steps = vec![
            json!({"step_index":0,"type":"USER_INPUT","status":"DONE","content":"<USER_REQUEST>\n继续\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nt=1\n</ADDITIONAL_METADATA>"}),
            json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","thinking":"Thought A from earlier.","tool_calls":[{"name":"read_file","args":{"Path":"a.lua"}}]}),
            json!({"step_index":2,"type":"GENERIC","status":"DONE","content":"ok"}),
            json!({"step_index":3,"type":"PLANNER_RESPONSE","status":"DONE","thinking":"Thought B from earlier.","content":"12:35 测试总结已完成。"}),
            json!({"step_index":4,"type":"USER_INPUT","status":"DONE","content":"<USER_REQUEST>\n你的后端是怎么mock的\n</USER_REQUEST>"}),
            json!({"step_index":5,"type":"PLANNER_RESPONSE","status":"DONE","content":"14:02 Mock 后端使用 OpenResty。"}),
            json!({"step_index":6,"type":"ERROR_MESSAGE","status":"DONE","content":"Error: The stream was interrupted. Please continue the task you were working on."}),
            json!({"step_index":7,"type":"USER_INPUT","status":"DONE","content":"<USER_REQUEST>\n继续\n</USER_REQUEST>"}),
            json!({"step_index":8,"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"write_file","args":{"Path":"b.lua"}}]}),
        ];
        write_agy_steps(root.path(), sid, &steps);

        let (hub, em, _rx) = crate::hub::test_emitter();
        let spec = agy_test_spec();
        let mut st = State {
            is_antigravity: true,
            session_id: Some(sid.into()),
            agy_last_streamed_steps: 0,
            ..Default::default()
        };

        // Start a new "继续" turn after resume; agy replays historical steps before echoing this prompt
        let prompt_req = prompt_with_brain_dir(&mut st, &em, &Prompt { text: "继续".into(), images: vec![] }, Some(root.path()));
        let prompt_id = prompt_req["id"].as_u64().unwrap();

        // Concatenated historical thoughts + historical tool call + concatenated replies + mid-replay "继续" + error + quota banner
        update(&mut st, &em, &json!({"sessionUpdate":"agent_thought_chunk","content":{"text":"Thought A from earlier.\nThought B from earlier."}}));
        update(&mut st, &em, &json!({"sessionUpdate":"tool_call","toolCallId":"call-old-1","name":"read_file","status":"in_progress","rawInput":{"Path":"a.lua"}}));
        update(&mut st, &em, &json!({"sessionUpdate":"tool_call_update","toolCallId":"call-old-1","status":"completed","rawOutput":{"output":"old"}}));
        update(&mut st, &em, &json!({"sessionUpdate":"agent_message_chunk","content":{"text":"12:35 测试总结已完成。14:02 Mock 后端使用 OpenResty。"}}));
        // Mid-replay USER_INPUT("继续") must NOT disarm the guard while steps[8] still follows
        update(&mut st, &em, &json!({"sessionUpdate":"user_message_chunk","content":{"text":"继续"}}));
        assert!(st.agy_replay.is_some(), "mid-replay historical user echo must not disarm the guard");
        update(&mut st, &em, &json!({"sessionUpdate":"agent_message_chunk","content":{"text":"⚠ The stream was interrupted. Please continue the task you were working on."}}));
        update(&mut st, &em, &json!({"sessionUpdate":"tool_call","toolCallId":"call-old-2","name":"write_file","status":"completed","rawInput":{"Path":"b.lua"}}));
        update(&mut st, &em, &json!({"sessionUpdate":"agent_message_chunk","content":{"text":"⚠ Individual quota reached. Please upgrade your subscription. Resets in 135h57m45s. ⚠ Individual quota reached. Please upgrade your subscription. Resets in 135h57m45s."}}));

        // Now current prompt echo arrives (after all historical USER_INPUTs) -> disarms guard
        update(&mut st, &em, &json!({"sessionUpdate":"user_message_chunk","content":{"text":"继续"}}));
        assert!(st.agy_replay.is_none(), "current prompt echo should disarm the guard");

        // Fresh reply arrives — even if its text matches an earlier reply, it must NOT be dropped
        update(&mut st, &em, &json!({"sessionUpdate":"agent_message_chunk","content":{"text":"14:02 Mock 后端使用 OpenResty。"}}));
        let _ = handle(&mut st, &spec, &json!({"jsonrpc":"2.0","id":prompt_id,"result":{"stopReason":"end_turn"}}), &em).unwrap();

        let evs = hub.store.events_since("s", 0, 100).unwrap();
        assert!(!evs.iter().any(|e| matches!(e.body, EventBody::Thinking { .. })), "replayed thinking leaked");
        assert!(!evs.iter().any(|e| matches!(e.body, EventBody::ToolCall { .. } | EventBody::ToolResult { .. })), "replayed tool leaked");
        assert!(!evs.iter().any(|e| matches!(e.body, EventBody::RateLimit { .. })), "replayed quota triggered fake rate_limit");
        let replies: Vec<&str> = evs.iter().filter_map(|e| match &e.body {
            EventBody::AssistantText { text, .. } => Some(text.as_str()),
            _ => None,
        }).collect();
        assert_eq!(replies, ["14:02 Mock 后端使用 OpenResty。"]);

        // Scenario 2: If session/prompt returns end_turn right after a replayed quota banner with no fresh reply, no fake rate_limit fires
        let (hub2, em2, _rx2) = crate::hub::test_emitter();
        let mut st2 = State {
            is_antigravity: true,
            session_id: Some(sid.into()),
            agy_last_streamed_steps: 0,
            ..Default::default()
        };
        let pid2 = prompt_with_brain_dir(&mut st2, &em2, &Prompt { text: "继续".into(), images: vec![] }, Some(root.path()))["id"].as_u64().unwrap();
        update(&mut st2, &em2, &json!({"sessionUpdate":"tool_call","toolCallId":"call-old-2","name":"write_file","status":"completed","rawInput":{"Path":"b.lua"}}));
        update(&mut st2, &em2, &json!({"sessionUpdate":"agent_message_chunk","content":{"text":"\n⚠ Individual quota reached. Resets in 135h57m45s."}}));
        let _ = handle(&mut st2, &spec, &json!({"jsonrpc":"2.0","id":pid2,"result":{"stopReason":"end_turn"}}), &em2).unwrap();
        let evs2 = hub2.store.events_since("s", 0, 100).unwrap();
        assert!(!evs2.iter().any(|e| matches!(e.body, EventBody::RateLimit { .. })), "replayed quota banner must not trigger rate_limit");
        assert!(!evs2.iter().any(|e| matches!(e.body, EventBody::AssistantText { .. })), "replayed quota banner must not be saved");
    }

    #[test]
    fn antigravity_cycle_end_then_grace_exit_still_resumes_and_ready_to_prompt_waits() {
        use super::*;
        let root = tempfile::tempdir().unwrap();
        let parent_id = "p-cycle-grace";
        let (hub, em, _rx) = crate::hub::test_emitter();
        let steps = vec![
            json!({"step_index":0,"type":"USER_INPUT","status":"DONE","content":"run two"}),
            json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{"CommandLine":"a"}},{"name":"run_command","args":{"CommandLine":"b"}}]}),
            json!({"step_index":2,"type":"GENERIC","status":"RUNNING","content":"Created At: x\nTool is running as a background task with task id: p-cycle-grace/task-a"}),
            json!({"step_index":3,"type":"GENERIC","status":"RUNNING","content":"Created At: x\nTool is running as a background task with task id: p-cycle-grace/task-b"}),
            json!({"step_index":4,"type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=x sender=p-cycle-grace/task-a priority=MESSAGE_PRIORITY_HIGH content=Task id \"p-cycle-grace/task-a\" finished with result:\nCommand exited with code 0.\n\nCommand output:\nA-OK\n</SYSTEM_MESSAGE>"}),
            json!({"step_index":5,"type":"PLANNER_RESPONSE","status":"DONE","content":"Task A finished while B runs."}),
        ];
        write_agy_steps(root.path(), parent_id, &steps);

        // Tool-only boundary check: trailing wakeup SYSTEM_MESSAGE must not be skipped when last_reply is empty
        assert_eq!(agy_prompt_boundary(&steps[..5], 0, ""), 4);

        let mut st = State { is_antigravity: true, session_id: Some(parent_id.into()), ..Default::default() };
        let mut watch = AgyWatch::new(parent_id.into(), root.path().to_path_buf(), 0, 4, Some("end_turn".into()), HashMap::new(), HashMap::new(), true);
        watch.detached = true;
        watch.add_bg_task(AgyBgTask { id: "p-cycle-grace/task-a".into(), kind: "local_bash".into(), description: "Task A".into(), started_at: 1, output_file: None, tc_id: Some("tc-a".into()), is_daemon: false, timer_cond: None, done: false, announced: false, result: None });
        watch.add_bg_task(AgyBgTask { id: "p-cycle-grace/task-b".into(), kind: "local_bash".into(), description: "Task B".into(), started_at: 2, output_file: None, tc_id: Some("tc-b".into()), is_daemon: false, timer_cond: None, done: false, announced: false, result: None });
        st.agy_watch = Some(watch);

        // Cycle 1 settles via CycleEnd (task-b still running, so watch.wake becomes false)
        assert!(poll_agy_watch(&mut st, &agy_test_spec(), &em).is_none());
        assert!(poll_agy_watch(&mut st, &agy_test_spec(), &em).is_none());
        assert!(!st.agy_watch.as_ref().unwrap().wake);
        assert!(!st.agy_watch.as_ref().unwrap().unresumed_items.is_empty());

        // Task B finishes silently (no second wakeup message) and WAKEUP_GRACE expires
        st.agy_watch.as_mut().unwrap().bg_tasks.iter_mut().for_each(|b| b.done = true);
        st.agy_watch.as_mut().unwrap().all_subagents_done_at =
            Some(tokio::time::Instant::now() - std::time::Duration::from_secs(25));
        let sync = poll_agy_watch(&mut st, &agy_test_spec(), &em)
            .expect("grace exit after CycleEnd must still send session/resume for cycle 1");
        assert_eq!(sync["method"], "session/resume");

        // While Pending::AgyResume is inflight, ready_to_prompt() must wait so session/prompt doesn't race it
        assert!(!st.ready_to_prompt(), "queued prompts must wait for Pending::AgyResume");
        let sync_id = sync["id"].as_u64().unwrap();
        let _ = handle(&mut st, &agy_test_spec(), &json!({"jsonrpc":"2.0","id":sync_id,"result":{}}), &em).unwrap();
        assert!(st.ready_to_prompt());
        assert!(st.agy_carry.unresumed_items.is_empty());
        assert_eq!(st.agy_last_streamed_steps, 6);
        let _ = hub;
    }

    #[tokio::test]
    async fn antigravity_deferred_restart_survives_task_end_until_wakeup_finishes() {
        use super::*;
        let root = tempfile::tempdir().unwrap();
        let parent_id = "p-bg-restart";
        let (hub, em, mut rx) = crate::hub::test_emitter();
        crate::hub::test_set_tool(&hub, "s", ToolKind::Antigravity);
        crate::hub::test_set_restart_pending(&hub, "s", true);

        let bg_notice = "Created At: x\nTool is running as a background task with task id: p-bg-restart/task-188\nTask Description: run e2e\nTask logs are available at: file:///tmp/task-188.log";
        let mut steps = vec![
            json!({"step_index":0,"type":"USER_INPUT","status":"DONE","content":"run e2e"}),
            json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{"CommandLine":"run e2e","toolSummary":"Run isolated E2E"}}]}),
            json!({"step_index":2,"type":"GENERIC","status":"RUNNING","content":bg_notice}),
            json!({"step_index":3,"type":"PLANNER_RESPONSE","status":"DONE","content":"Running E2E in background."}),
        ];
        write_agy_steps(root.path(), parent_id, &steps);

        let mut st = State { is_antigravity: true, session_id: Some(parent_id.into()), ..Default::default() };
        let mut watch = AgyWatch::new(parent_id.into(), root.path().to_path_buf(), 0, 4, Some("end_turn".into()), HashMap::new(), HashMap::new(), true);
        watch.detached = true;
        watch.add_bg_task(AgyBgTask {
            id: "p-bg-restart/task-188".into(),
            kind: "local_bash".into(),
            description: "Run isolated E2E".into(),
            started_at: 1,
            output_file: Some("/tmp/task-188.log".into()),
            tc_id: Some("tc-188".into()),
            is_daemon: false,
            timer_cond: None,
            done: false,
            announced: false,
            result: None,
        });
        st.agy_watch = Some(watch);
        let spec = agy_test_spec();

        // Turn ends while task-188 is still running -> watch_active stays true, restart_pending held
        assert!(poll_agy_watch(&mut st, &spec, &em).is_none());
        em.event(EventBody::TurnEnd { stop_reason: Some("end_turn".into()), model: None });
        assert_eq!(hub.info("s").unwrap().status, SessionStatus::Idle);
        assert_eq!(hub.info("s").unwrap().background_tasks.len(), 1);
        assert!(crate::hub::test_watch_active(&hub, "s"));
        assert!(crate::hub::test_restart_pending(&hub, "s"));

        // Task-188 fails and writes SYSTEM_MESSAGE, BEFORE parent writes wakeup PLANNER_RESPONSE
        steps.push(json!({
            "step_index": 4,
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "content": "<SYSTEM_MESSAGE>\n[Message] timestamp=x sender=p-bg-restart/task-188 priority=MESSAGE_PRIORITY_HIGH content=Task id \"p-bg-restart/task-188\" finished with result:\nCommand exited with code 1.\n\nCommand output:\nTimeout 15000ms exceeded\nLog: file:///tmp/task-188.log\n</SYSTEM_MESSAGE>"
        }));
        write_agy_steps(root.path(), parent_id, &steps);

        assert!(poll_agy_watch(&mut st, &spec, &em).is_none());
        tokio::task::yield_now().await;
        // background_tasks cleared in UI, but watch_active keeps restart_pending from killing agy mid-wakeup!
        assert!(hub.info("s").unwrap().background_tasks.is_empty());
        assert!(crate::hub::test_watch_active(&hub, "s"));
        assert!(crate::hub::test_restart_pending(&hub, "s"), "deferred restart must wait for agy wakeup reply");
        assert!(rx.try_recv().is_err(), "agy process must not be killed before waking up");

        // Parent wakes up and reports the failure
        steps.push(json!({
            "step_index": 5,
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "content": "E2E timed out; fixing and retrying."
        }));
        write_agy_steps(root.path(), parent_id, &steps);

        assert!(poll_agy_watch(&mut st, &spec, &em).is_none());
        assert_eq!(hub.info("s").unwrap().status, SessionStatus::Running);
        let sync = poll_agy_watch(&mut st, &spec, &em).expect("wakeup finished with session/resume");
        assert_eq!(sync["method"], "session/resume");
        assert!(!crate::hub::test_watch_active(&hub, "s"));
        assert!(!crate::hub::test_restart_pending(&hub, "s"), "after_turn should run deferred restart once wakeup finishes");

        let evs = hub.store.events_since("s", 0, 100).unwrap();
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::System { text } if text == "后台任务失败：Run isolated E2E")));
        assert!(evs.iter().any(|e| matches!(&e.body, EventBody::AssistantText { text, .. } if text == "E2E timed out; fixing and retrying.")));
    }

    #[test]
    fn antigravity_parallel_background_command_discovered_behind_unsettled_step() {
        use super::*;
        let (hub, em, _rx) = crate::hub::test_emitter();
        let mut st = State {
            is_antigravity: true,
            session_id: Some("p-par".into()),
            ..Default::default()
        };
        update(
            &mut st,
            &em,
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-sync-1",
                "name": "run_command",
                "title": "slow sync",
                "status": "in_progress",
                "rawInput": {"CommandLine": "sleep 8", "toolSummary": "Slow sync cmd"}
            }),
        );
        update(
            &mut st,
            &em,
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-bg-2",
                "name": "run_command",
                "title": "fast bg",
                "status": "in_progress",
                "rawInput": {"CommandLine": "npm run build", "toolSummary": "Parallel bg build"}
            }),
        );
        let steps = vec![
            json!({"step_index":0,"type":"USER_INPUT","status":"DONE","content":"go"}),
            json!({"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[
                {"name":"run_command","args":{"CommandLine":"sleep 8","toolSummary":"Slow sync cmd"}},
                {"name":"run_command","args":{"CommandLine":"npm run build","toolSummary":"Parallel bg build"}}
            ]}),
            // First command still inside WaitMsBeforeAsync (unsettled GENERIC/RUNNING without background notice)
            json!({"step_index":2,"type":"GENERIC","status":"RUNNING","content":"Created At: x\nRunning..."}),
            // Second command already moved to background
            json!({"step_index":3,"type":"GENERIC","status":"RUNNING","content":"Created At: x\nTool is running as a background task with task id: p-par/task-3\nTask Description: npm run build\nTask logs are available at: file:///tmp/t3.log"}),
        ];

        assert!(discover_agy_bg_from_steps(&mut st, &steps));
        assert_eq!(st.agy_bg_tasks.len(), 1);
        assert_eq!(st.agy_bg_tasks[0].id, "p-par/task-3");
        assert_eq!(st.agy_bg_tasks[0].description, "Parallel bg build");
        assert_eq!(st.agy_bg_tasks[0].tc_id.as_deref(), Some("tc-bg-2"), "must bind the matching parallel toolCallId, not tc-sync-1");
        assert_eq!(st.agy_open_bg_calls.len(), 1);
        assert_eq!(st.agy_open_bg_calls[0].0, "tc-sync-1");
        let _ = hub;
    }

    #[test]
    fn antigravity_replay_guard_handles_out_of_order_parallel_tools() {
        use super::*;
        let history = vec![
            AgyHistoryItem::Tool { name: "manage_task".into(), input: json!({"Action":"status","TaskId":"t1"}) },
            AgyHistoryItem::Tool { name: "run_command".into(), input: json!({"CommandLine":"git diff"}) },
            AgyHistoryItem::Tool { name: "manage_task".into(), input: json!({"Action":"status","TaskId":"t2"}) },
        ];
        let mut guard = AgyReplayGuard::new("继续", VecDeque::new(), history).unwrap();
        // agy replays parallel tool calls [0, 2, 1] out of declaration order
        assert!(guard.consume_tool("manage_task", &json!({"Action":"status","TaskId":"t1"})));
        assert!(guard.consume_tool("manage_task", &json!({"Action":"status","TaskId":"t2"})));
        assert!(guard.consume_tool("run_command", &json!({"CommandLine":"git diff"})), "skipped parallel tool must still match once");
        // Each historical tool can only be consumed once
        assert!(!guard.consume_tool("run_command", &json!({"CommandLine":"git diff"})));
    }

    fn write_agy_transcript(brain_dir: &std::path::Path, sid: &str, steps: &[Value]) {
        let logs = brain_dir.join(sid).join(".system_generated").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let body = steps.iter().map(|s| serde_json::to_string(s).unwrap()).collect::<Vec<_>>().join("\n");
        std::fs::write(logs.join("transcript_full.jsonl"), format!("{body}\n")).unwrap();
    }

    #[test]
    fn antigravity_manual_compact_emits_progress_resets_context_and_rearms_memory() {
        let root = tempfile::tempdir().unwrap();
        let sid = "agy-compact-manual-bare";
        let pre = vec![
            json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"<USER_REQUEST>\nhi\n</USER_REQUEST>"}),
            json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","input_tokens":12989,"cache_read_tokens":0,"output_tokens":23,"content":"OK"}),
        ];
        write_agy_transcript(root.path(), sid, &pre);

        let (hub, em, _rx) = crate::hub::test_emitter();
        crate::hub::test_set_tool(&hub, "s", ToolKind::Antigravity);
        let mut spec = agy_test_spec();
        spec.memory_prompt = Some("<yi-memory>facts</yi-memory>".into());
        let mut st = State {
            is_antigravity: true,
            memory_enabled: true,
            session_id: Some(sid.into()),
            ..Default::default()
        };

        let req = prompt_with_brain_dir(&mut st, &em, &Prompt { text: "/compact".into(), images: vec![] }, Some(root.path()));
        assert_eq!(req["params"]["prompt"][0]["text"], "/compact");
        assert!(st.agy_compact.is_some());

        // During bare /compact, agy emits usage_update for the summarizer LLM call (pre-compact size)
        update(&mut st, &em, &json!({"sessionUpdate":"usage_update","used":14474,"size":1048576}));
        assert!(hub.info("s").unwrap().context_percent.unwrap() > 1.0);

        let mut post = pre.clone();
        post.push(json!({"step_index":2,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"<USER_REQUEST>\n/compact\n</USER_REQUEST>"}));
        post.push(json!({"step_index":3,"source":"SYSTEM","type":"CHECKPOINT","status":"DONE","input_tokens":2990,"cache_read_tokens":10932,"output_tokens":552,"content":"# Resuming from a compaction"}));
        st.agy_parent_transcript.steps = std::sync::Arc::new(post);

        handle(&mut st, &spec, &json!({"id":req["id"],"result":{"stopReason":"end_turn"}}), &em).unwrap();
        assert!(st.agy_compact.is_none());
        assert_eq!(hub.info("s").unwrap().context_percent, Some(0.0), "bare /compact resets stale summarizer context %");
        assert!(
            st.memory_notice.as_deref().is_some_and(|n| n.contains("<system-reminder>") && n.contains("<yi-memory")),
            "memory notice must re-arm: {:?}",
            st.memory_notice
        );

        let compacts: Vec<_> = hub.store.events_since("s", 0, 100).unwrap().into_iter().filter_map(|e| match e.body {
            EventBody::Compact { phase, trigger, pre_tokens, post_tokens, duration_ms } => Some((phase, trigger, pre_tokens, post_tokens, duration_ms.is_some())),
            _ => None,
        }).collect();
        assert_eq!(compacts, vec![
            ("start".into(), Some("manual".into()), None, None, false),
            ("done".into(), Some("manual".into()), Some(13012), None, true),
        ]);
    }

    #[test]
    fn antigravity_manual_compact_with_instructions_keeps_post_tokens_and_context() {
        let root = tempfile::tempdir().unwrap();
        let sid = "agy-compact-manual-extra";
        let pre = vec![
            json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"<USER_REQUEST>\nRemember BANANA\n</USER_REQUEST>"}),
            json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","input_tokens":12996,"cache_read_tokens":0,"output_tokens":25,"content":"OK"}),
        ];
        write_agy_transcript(root.path(), sid, &pre);

        let (hub, em, _rx) = crate::hub::test_emitter();
        crate::hub::test_set_tool(&hub, "s", ToolKind::Antigravity);
        let spec = agy_test_spec();
        let mut st = State { is_antigravity: true, session_id: Some(sid.into()), ..Default::default() };

        let req = prompt_with_brain_dir(&mut st, &em, &Prompt { text: "/compact keep BANANA".into(), images: vec![] }, Some(root.path()));
        update(&mut st, &em, &json!({"sessionUpdate":"usage_update","used":14325,"size":1048576}));
        update(&mut st, &em, &json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Context compacted."}}));
        update(&mut st, &em, &json!({"sessionUpdate":"usage_update","used":13440,"size":1048576}));

        let mut post = pre.clone();
        post.push(json!({"step_index":2,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"<USER_REQUEST>\n/compact keep BANANA\n</USER_REQUEST>"}));
        post.push(json!({"step_index":3,"source":"SYSTEM","type":"CHECKPOINT","status":"DONE","input_tokens":2998,"cache_read_tokens":10939,"output_tokens":388,"content":"# Resuming from a compaction"}));
        post.push(json!({"step_index":4,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","input_tokens":13371,"cache_read_tokens":0,"output_tokens":69,"content":"Context compacted."}));
        st.agy_parent_transcript.steps = std::sync::Arc::new(post);

        handle(&mut st, &spec, &json!({"id":req["id"],"result":{"stopReason":"end_turn"}}), &em).unwrap();
        let pct = hub.info("s").unwrap().context_percent.unwrap();
        assert_eq!(pct, 1.3, "post-checkpoint usage_update must be preserved (13440 / 1048576 -> 1.3%)");

        let compacts: Vec<_> = hub.store.events_since("s", 0, 100).unwrap().into_iter().filter_map(|e| match e.body {
            EventBody::Compact { phase, trigger, pre_tokens, post_tokens, .. } => Some((phase, trigger, pre_tokens, post_tokens)),
            _ => None,
        }).collect();
        assert_eq!(compacts, vec![
            ("start".into(), Some("manual".into()), None, None),
            ("done".into(), Some("manual".into()), Some(13021), Some(13371)),
        ]);
    }

    #[test]
    fn antigravity_compact_empty_session_and_failure_paths() {
        let root = tempfile::tempdir().unwrap();
        let spec = agy_test_spec();

        // 1. Empty session: no pre-turn history, agy replies normally without writing a CHECKPOINT
        let (hub, em, _rx) = crate::hub::test_emitter();
        crate::hub::test_set_tool(&hub, "s", ToolKind::Antigravity);
        let mut st = State { is_antigravity: true, session_id: Some("agy-empty".into()), ..Default::default() };
        let req = prompt_with_brain_dir(&mut st, &em, &Prompt { text: "/compact".into(), images: vec![] }, Some(root.path()));
        assert!(st.agy_compact.is_none(), "empty session must not emit a fake compact start");
        st.agy_parent_transcript.steps = std::sync::Arc::new(vec![
            json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"/compact"}),
            json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"Nothing to compact yet."}),
        ]);
        handle(&mut st, &spec, &json!({"id":req["id"],"result":{"stopReason":"end_turn"}}), &em).unwrap();
        assert!(hub.store.events_since("s", 0, 100).unwrap().iter().all(|e| !matches!(e.body, EventBody::Compact { .. })));

        // 2. Non-empty session where /compact fails (error, cancelled, or end_turn with no CHECKPOINT)
        let sid = "agy-fail";
        let pre = vec![
            json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"hi"}),
            json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","input_tokens":5000,"cache_read_tokens":0,"output_tokens":50,"content":"OK"}),
        ];
        write_agy_transcript(root.path(), sid, &pre);
        for resp in [
            json!({"error":{"code":-32603,"message":"Internal error"}}),
            json!({"result":{"stopReason":"cancelled"}}),
            json!({"result":{"stopReason":"end_turn"}}),
        ] {
            let (hub, em, _rx) = crate::hub::test_emitter();
            crate::hub::test_set_tool(&hub, "s", ToolKind::Antigravity);
            let mut st = State { is_antigravity: true, session_id: Some(sid.into()), ..Default::default() };
            let req = prompt_with_brain_dir(&mut st, &em, &Prompt { text: "/compact".into(), images: vec![] }, Some(root.path()));
            st.agy_parent_transcript.steps = std::sync::Arc::new(pre.clone());
            let mut msg = resp.clone();
            msg["id"] = req["id"].clone();
            handle(&mut st, &spec, &msg, &em).unwrap();
            assert!(st.agy_compact.is_none());
            let compacts: Vec<_> = hub.store.events_since("s", 0, 100).unwrap().into_iter().filter_map(|e| match e.body {
                EventBody::Compact { phase, trigger, pre_tokens, .. } => Some((phase, trigger, pre_tokens)),
                _ => None,
            }).collect();
            assert_eq!(compacts, vec![
                ("start".into(), Some("manual".into()), None),
                ("failed".into(), Some("manual".into()), Some(5050)),
            ], "failed response: {resp}");
        }
    }

    #[test]
    fn antigravity_auto_compaction_in_prompt_and_wakeup() {
        let root = tempfile::tempdir().unwrap();
        let mut spec = agy_test_spec();
        spec.memory_prompt = Some("<yi-memory>memo</yi-memory>".into());

        // 1. Auto-compaction during a normal prompt turn
        let sid = "agy-auto-prompt";
        let pre = vec![
            json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"start"}),
            json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","input_tokens":180000,"cache_read_tokens":20000,"output_tokens":500,"content":"working"}),
        ];
        write_agy_transcript(root.path(), sid, &pre);
        let (hub, em, _rx) = crate::hub::test_emitter();
        crate::hub::test_set_tool(&hub, "s", ToolKind::Antigravity);
        let mut st = State { is_antigravity: true, memory_enabled: true, session_id: Some(sid.into()), ..Default::default() };
        let req = prompt_with_brain_dir(&mut st, &em, &Prompt { text: "continue".into(), images: vec![] }, Some(root.path()));
        let mut post = pre.clone();
        post.push(json!({"step_index":2,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"continue"}));
        post.push(json!({"step_index":3,"source":"SYSTEM","type":"CHECKPOINT","status":"DONE","input_tokens":5000,"cache_read_tokens":190000,"output_tokens":1200,"content":"# Resuming from a compaction"}));
        post.push(json!({"step_index":4,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","input_tokens":15000,"cache_read_tokens":9000,"output_tokens":200,"content":"done"}));
        st.agy_parent_transcript.steps = std::sync::Arc::new(post);
        handle(&mut st, &spec, &json!({"id":req["id"],"result":{"stopReason":"end_turn"}}), &em).unwrap();
        assert!(st.memory_notice.as_deref().is_some_and(|n| n.contains("<system-reminder>") && n.contains("<yi-memory")));
        let compacts: Vec<_> = hub.store.events_since("s", 0, 100).unwrap().into_iter().filter_map(|e| match e.body {
            EventBody::Compact { phase, trigger, pre_tokens, post_tokens, .. } => Some((phase, trigger, pre_tokens, post_tokens)),
            _ => None,
        }).collect();
        assert_eq!(compacts, vec![("done".into(), Some("auto".into()), Some(200500), Some(24000))]);

        // 2. Auto-compaction during an AgyWatch parent wakeup turn
        let wsid = "agy-auto-wakeup";
        let wake_steps = vec![
            json!({"step_index":0,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","input_tokens":150000,"cache_read_tokens":0,"output_tokens":100,"content":"waiting"}),
            json!({"step_index":1,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","content":"<SYSTEM_MESSAGE>\n[Message] sender=sub-1 content=finished\n</SYSTEM_MESSAGE>"}),
            json!({"step_index":2,"source":"SYSTEM","type":"CHECKPOINT","status":"DONE","input_tokens":4000,"cache_read_tokens":145000,"output_tokens":800,"content":"# Resuming from a compaction"}),
            json!({"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","input_tokens":22000,"cache_read_tokens":0,"output_tokens":50,"content":"All done."}),
        ];
        write_agy_transcript(root.path(), wsid, &wake_steps);
        let (hub, em, _rx) = crate::hub::test_emitter();
        crate::hub::test_set_tool(&hub, "s", ToolKind::Antigravity);
        let mut st = State { is_antigravity: true, memory_enabled: true, session_id: Some(wsid.into()), ..Default::default() };
        let mut watch = AgyWatch::new(wsid.into(), root.path().to_path_buf(), 0, 1, Some("end_turn".into()), HashMap::new(), HashMap::new(), true);
        watch.detached = true;
        st.agy_watch = Some(watch);
        let _ = poll_agy_watch(&mut st, &spec, &em);
        assert!(st.memory_notice.as_deref().is_some_and(|n| n.contains("<system-reminder>") && n.contains("<yi-memory")));
        let compacts: Vec<_> = hub.store.events_since("s", 0, 100).unwrap().into_iter().filter_map(|e| match e.body {
            EventBody::Compact { phase, trigger, pre_tokens, post_tokens, .. } => Some((phase, trigger, pre_tokens, post_tokens)),
            _ => None,
        }).collect();
        assert_eq!(compacts, vec![("done".into(), Some("auto".into()), Some(150100), Some(22000))]);
    }
}
