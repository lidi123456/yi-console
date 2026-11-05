//! Session-scoped desktop tools. Providers can reach only this gateway, never the driver's transport.
use anyhow::{anyhow, bail, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{watch, Mutex as AsyncMutex};
use yi_proto::{EventBody, ImageRef};

const VERSION: &str = "0.34.0";
const BINARY_SHA: &str = "77f5cac754b42b6a8bae126414fc8f7487432ace93466967965188e24e9e53fb";
const SERVER: &str = "yi_computer_use";
/// The whole desktop, as in OpenBot. Left out: browser CDP tools (use the browser tools), recording/replay, driver
/// sessions, config, cursor themes, installers/updates, diagnostics, and kill_app (close a window instead).
const READ_TOOLS: &[&str] = &[
    "list_apps",
    "list_windows",
    "get_window_state",
    "verify_state",
    "zoom",
    "get_desktop_state",
    "get_screen_size",
    "get_cursor_position",
    "get_accessibility_tree",
    "clipboard_read",
];
const WRITE_TOOLS: &[&str] = &[
    "launch_app",
    "bring_to_front",
    "set_window_frame",
    "invoke_menu",
    "click",
    "double_click",
    "right_click",
    "drag",
    "type_text",
    "press_key",
    "hotkey",
    "set_value",
    "scroll",
    "move_cursor",
    "clipboard_write",
];

/// Every desktop tool the gateway exposes (Kiro's trust list needs the names)
pub fn tool_names() -> impl Iterator<Item = &'static str> {
    READ_TOOLS.iter().chain(WRITE_TOOLS).copied()
}
/// Shared by the MCP initialize instructions, the CLI system prompts and the ACP first-prompt notice.
/// Modeled on OpenBot's guidance: short path, few reads, read structuredContent, no workarounds.
macro_rules! guide {
    () => {
        "You may operate any app on this Windows desktop; act only on the windows the task needs. \
Find windows with list_windows (or launch_app to open one) and choose by title and size, never the first match: many windows are small empty helpers. \
Pass that window's pid and window_id to every window tool. \
Take the shortest path: read a window once with get_window_state, then act. To find one element, pass query (and include_screenshot: false when you only need its element_token); \
do not read the whole window again after every action. Prefer element_token from the latest read; x and y are pixels of the latest screenshot of that window. \
After each action read its structuredContent: refused or suspected_noop means it did not happen; unverifiable means check once, with a query for the expected text, before you report it as done. \
Chromium and Electron windows (Feishu, Slack, VS Code, browsers) drop background input: use delivery_mode \"foreground\" there from the first input; it briefly takes the focus. \
To submit typed text, press_key return is more reliable than clicking a send button. \
Before an action that is hard to undo (sending to other people, deleting, paying, closing unsaved work), make sure the user asked for it. \
A paused, stopped or busy error means the user took control: stop and tell them. \
Never start your own MCP client, read Yi Console ticket files, or drive the desktop through shell scripts or other desktop tools. \
When a direct API, CLI, file or browser tool can do the task, use that instead."
    };
}
const GUIDE: &str = guide!();

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub pid: u64,
    pub window_id: u64,
    pub title: String,
}

#[derive(Clone)]
struct Grant {
    ticket: String,
    paused: bool,
    /// Element tokens and capture id from the latest read of each window (0: the full desktop)
    tokens: HashMap<u64, HashSet<String>>,
    captures: HashMap<u64, String>,
    /// Window titles seen in list_windows, for the timeline
    titles: HashMap<u64, String>,
    /// The window the last action went to, for the status line
    last: Option<Target>,
    /// The proxy process allowed to use the ticket, and the process that started it (the AI tool)
    proxy: Option<(u32, Option<u32>)>,
}

#[derive(Default)]
struct Control {
    grants: HashMap<String, Grant>,
    owner: Option<String>,
}

pub struct Desktop {
    control: Mutex<Control>,
    driver: AsyncMutex<Option<Peer>>,
    cancel: watch::Sender<u64>,
}

impl Desktop {
    pub fn new() -> Self {
        let (cancel, _) = watch::channel(0);
        Self {
            control: Mutex::new(Control::default()),
            driver: AsyncMutex::new(None),
            cancel,
        }
    }

    pub fn status(&self, sid: Option<&str>) -> Value {
        let control = self.control.lock().unwrap();
        let grant = sid.and_then(|s| control.grants.get(s));
        json!({
            "supported": cfg!(all(windows, target_arch = "x86_64")), "driverVersion": VERSION,
            "status": if !cfg!(all(windows, target_arch = "x86_64")) { "unsupported" }
                else if !desktop_available() { "desktopUnavailable" }
                else if grant.is_some_and(|g| g.paused) { "paused" }
                else if control.owner.as_deref().is_some_and(|owner| Some(owner) != sid) { "busy" }
                else if grant.is_some() { "ready" } else { "disabled" },
            "enabled": grant.is_some(), "paused": grant.is_some_and(|g| g.paused),
            "ownerSessionId": control.owner, "lastWindow": grant.and_then(|g| g.last.as_ref()),
        })
    }

    fn ticket_path(sid: &str) -> PathBuf {
        crate::paths::base_dir()
            .join("computer-use")
            .join(format!("{sid}.json"))
    }

    pub fn configure(&self, sid: &str) -> Result<()> {
        if !cfg!(all(windows, target_arch = "x86_64")) {
            bail!("操作电脑首版仅支持 Windows x64");
        }
        if !desktop_available() {
            bail!("桌面不可用，请在已登录且解锁的 Windows 用户会话中运行 Yi Console");
        }
        let mut control = self.control.lock().unwrap();
        if control.owner.as_deref().is_some_and(|owner| owner != sid) {
            bail!("桌面正在被其他会话使用，请先在该会话里关闭操作电脑");
        }
        // Replacing a grant invalidates old proxies even when their CLI has not exited yet.
        let grant = Grant {
            ticket: uuid::Uuid::new_v4().to_string(),
            paused: false,
            tokens: HashMap::new(),
            captures: HashMap::new(),
            titles: HashMap::new(),
            last: None,
            proxy: None,
        };
        let path = Self::ticket_path(sid);
        std::fs::create_dir_all(path.parent().unwrap())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                path.parent().unwrap(),
                std::fs::Permissions::from_mode(0o700),
            )?;
        }
        std::fs::write(&path, serde_json::to_vec(&json!({"ticket": grant.ticket}))?)?;
        control.grants.insert(sid.into(), grant);
        control.owner = Some(sid.into());
        self.cancel.send_modify(|n| *n += 1);
        Ok(())
    }

    /// A new tool process will start its own proxy: forget the old one
    pub fn release_proxy(&self, sid: &str) {
        if let Some(grant) = self.control.lock().unwrap().grants.get_mut(sid) {
            grant.proxy = None;
        }
    }

    /// MCP traffic from a proxy process. The ticket file is readable by the model's own shell, so the ticket alone
    /// is not enough: it binds to the first proxy, which the AI tool starts. Another process may use it only when the
    /// same tool started it, or when the bound proxy has exited (the tool restarted its MCP servers).
    pub async fn proxy_call(&self, hub: &crate::hub::Hub, sid: &str, ticket: &str, packet: Value, pid: Option<u32>) -> Result<Value> {
        self.grant(sid, ticket, false)?;
        let pid = pid.ok_or_else(|| anyhow!("无法确认桌面代理进程"))?;
        {
            let mut control = self.control.lock().unwrap();
            let grant = control.grants.get_mut(sid).ok_or_else(|| anyhow!("操作电脑已关闭，请由用户重新开启"))?;
            let parent = process::parent(pid);
            match proxy_access(grant.proxy, pid, parent, process::alive) {
                ProxyAccess::Deny => bail!("操作电脑只接受 AI 工具自己启动的桌面代理，请直接调用 yi_computer_use 工具"),
                ProxyAccess::Bind => grant.proxy = Some((pid, parent)),
                ProxyAccess::Allow => {}
            }
        }
        self.mcp(hub, sid, ticket, packet).await
    }

    pub fn change(&self, sid: &str, action: &str) -> Result<()> {
        let mut control = self.control.lock().unwrap();
        let was_owner = control.owner.as_deref() == Some(sid);
        match action {
            "disable" | "stop" => {
                control.grants.remove(sid);
                let _ = std::fs::remove_file(Self::ticket_path(sid));
                if control.owner.as_deref() == Some(sid) {
                    control.owner = None;
                }
            }
            "pause" => {
                let grant = control
                    .grants
                    .get_mut(sid)
                    .ok_or_else(|| anyhow!("当前会话未开启操作电脑"))?;
                grant.paused = true;
                grant.tokens.clear();
                grant.captures.clear();
                if control.owner.as_deref() == Some(sid) {
                    control.owner = None;
                }
            }
            "resume" => {
                if control.owner.as_deref().is_some_and(|owner| owner != sid) {
                    bail!("桌面正在被其他会话使用");
                }
                control
                    .grants
                    .get_mut(sid)
                    .ok_or_else(|| anyhow!("当前会话未开启操作电脑"))?
                    .paused = false;
                control.owner = Some(sid.into());
            }
            _ => bail!("未知的操作电脑控制命令"),
        }
        if was_owner || action == "resume" {
            self.cancel.send_modify(|n| *n += 1);
        }
        Ok(())
    }

    /// Stopping a reply pauses the grant; the user's next message is the go-ahead again (no resume button, as in OpenBot).
    /// Stays paused if another session has taken the desktop meanwhile.
    pub fn resume_if_paused(&self, sid: &str) {
        let paused = self.control.lock().unwrap().grants.get(sid).is_some_and(|g| g.paused);
        if paused { let _ = self.change(sid, "resume"); }
    }

    /// Enabling is the user's authorization; avoid asking again for every MCP click.
    pub fn authorizes(&self, sid: &str, name: &str) -> bool {
        let Some(tool) = name.strip_prefix("mcp__yi_computer_use__") else {
            return false;
        };
        let control = self.control.lock().unwrap();
        allowed(tool)
            && control.owner.as_deref() == Some(sid)
            && control.grants.get(sid).is_some_and(|g| !g.paused)
    }

    pub fn mcp_config(&self, sid: &str) -> Option<Value> {
        let control = self.control.lock().unwrap();
        control.grants.get(sid)?;
        Some(json!({"command": std::env::current_exe().ok()?.to_string_lossy(), "args": proxy_args(std::env::var_os("YI_CONSOLE_HOME"), sid, &Self::ticket_path(sid))}))
    }

    fn grant(&self, sid: &str, ticket: &str, active: bool) -> Result<Grant> {
        let control = self.control.lock().unwrap();
        let grant = control
            .grants
            .get(sid)
            .ok_or_else(|| anyhow!("操作电脑已关闭，请由用户重新开启"))?;
        if grant.ticket != ticket {
            bail!("操作电脑连接已失效，请重连会话");
        }
        if active && (grant.paused || control.owner.as_deref() != Some(sid)) {
            bail!("操作电脑已暂停或桌面被占用，请由用户恢复");
        }
        Ok(grant.clone())
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_for(method, params, None, None).await
    }

    async fn request_for(
        &self,
        method: &str,
        params: Value,
        auth: Option<(&str, &str)>,
        hub: Option<&crate::hub::Hub>,
    ) -> Result<Value> {
        let mut cancel = self.cancel.subscribe();
        let mut lock = self.driver.lock().await;
        if let Some((sid, ticket)) = auth {
            let grant = self.grant(sid, ticket, true)?;
            let tool = params["name"].as_str().unwrap_or("");
            validate_snapshot(&params["arguments"], &grant, tool)?;
            if WRITE_TOOLS.contains(&tool)
                && hub.is_some_and(|h| {
                    h.info(sid)
                        .is_ok_and(|info| info.permission_mode.as_deref() == Some("plan"))
                })
            {
                bail!("只读规划模式不能操作桌面输入");
            }
        }
        if !cfg!(all(windows, target_arch = "x86_64")) {
            bail!("操作电脑首版仅支持 Windows x64");
        }
        if !desktop_available() {
            bail!("桌面不可用，请在已登录且解锁的 Windows 用户会话中运行 Yi Console");
        }
        if cancel.has_changed().unwrap_or(true) {
            bail!("桌面控制状态已变化，请重新读取窗口");
        }
        if lock.is_none() {
            *lock = Some(
                tokio::time::timeout(Duration::from_secs(15), Peer::start())
                    .await
                    .map_err(|_| anyhow!("桌面驱动启动超时"))??,
            );
        }
        let peer = lock.as_mut().unwrap();
        let result = tokio::select! {
            biased;
            _ = cancel.changed() => Err(anyhow!("桌面操作已中断，结果待核验，请重新读取窗口")),
            response = tokio::time::timeout(Duration::from_secs(25), peer.request(method, params)) => response.unwrap_or_else(|_| Err(anyhow!("桌面操作超时，结果待核验，请读取窗口后再继续"))),
        };
        if result.is_err() {
            if let Some(mut peer) = lock.take() {
                let _ = peer.child.kill().await;
            }
        }
        result
    }

    /// Await native shutdown before a pause/stop response; never stop a new desktop owner.
    pub async fn close_if_idle(&self) {
        let mut driver = self.driver.lock().await;
        let idle = self.control.lock().unwrap().owner.is_none();
        if idle {
            if let Some(mut peer) = driver.take() {
                let _ = peer.child.kill().await;
            }
        }
    }

    pub async fn capture(&self, hub: &crate::hub::Hub, sid: &str) -> Result<()> {
        let ticket = self
            .control
            .lock()
            .unwrap()
            .grants
            .get(sid)
            .ok_or_else(|| anyhow!("当前会话未开启操作电脑"))?
            .ticket
            .clone();
        let result = self.mcp(hub, sid, &ticket, json!({"method":"tools/call","params":{"name":"get_desktop_state","arguments":{"max_image_dimension":1600}}})).await?;
        if result["isError"] == true {
            bail!("无法截取桌面，请确认 Windows 已登录并解锁");
        }
        Ok(())
    }

    pub async fn targets(&self) -> Result<Vec<Target>> {
        let response = self
            .request("tools/call", json!({"name":"list_windows","arguments":{}}))
            .await;
        self.close_if_idle().await;
        let response = response?;
        if response["isError"] == true {
            bail!("无法读取桌面窗口，请确认已登录并解锁 Windows 桌面");
        }
        let value = structured(&response);
        let mut found = vec![];
        collect_targets(&value, None, &mut found);
        found.sort_by(|a, b| a.title.cmp(&b.title));
        found.dedup_by_key(|t| t.window_id);
        if found.is_empty() {
            bail!("未发现可操作窗口，请确认 daemon 运行在已登录的 Windows 桌面中");
        }
        Ok(found)
    }

    pub async fn mcp(
        &self,
        hub: &crate::hub::Hub,
        sid: &str,
        ticket: &str,
        packet: Value,
    ) -> Result<Value> {
        let grant = self.grant(sid, ticket, false)?;
        let method = packet["method"].as_str().unwrap_or("");
        let params = &packet["params"];
        match method {
            "initialize" => Ok(
                json!({"protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "serverInfo":{"name":SERVER,"version":"1"},"capabilities":{"tools":{},"resources":{}},"instructions":GUIDE}),
            ),
            "tools/list" => {
                let mut response = self.request("tools/list", json!({})).await?;
                if let Some(tools) = response["tools"].as_array_mut() {
                    tools.retain(|tool| tool["name"].as_str().is_some_and(allowed));
                    tools.iter_mut().for_each(hide_gateway_args);
                }
                Ok(response)
            }
            "resources/list" => Ok(
                json!({"resources":[{"uri":"yi://computer-use/guide","name":"Computer Use guide","mimeType":"text/plain"}]}),
            ),
            "resources/read" if params["uri"] == "yi://computer-use/guide" => Ok(
                json!({"contents":[{"uri":"yi://computer-use/guide","mimeType":"text/plain","text":GUIDE}]}),
            ),
            "resources/templates/list" => Ok(json!({"resourceTemplates":[]})),
            "ping" => Ok(json!({})),
            "tools/call" => {
                let tool = params["name"]
                    .as_str()
                    .ok_or_else(|| anyhow!("缺少工具名称"))?;
                if !allowed(tool) {
                    bail!("该桌面工具不在当前会话允许范围内");
                }
                self.grant(sid, ticket, true)?;
                let info = hub.info(sid)?;
                if WRITE_TOOLS.contains(&tool) && info.permission_mode.as_deref() == Some("plan") {
                    bail!("只读规划模式不能操作桌面输入");
                }
                let mut args = params["arguments"].as_object().cloned().unwrap_or_default();
                args.remove("__tool_use_purpose");
                if serde_json::to_vec(&args)?.len() > 64 * 1024 {
                    bail!("桌面工具参数过大");
                }
                // An agent cannot attach its request to a different driver lifecycle session.
                args.remove("session");
                if args.contains_key("screenshot_out_file") {
                    bail!("截图只能保存在当前会话的产物目录");
                }
                validate_snapshot(&json!(args), &grant, tool)?;
                let window = args.get("window_id").and_then(Value::as_u64);
                let label = window
                    .and_then(|w| grant.titles.get(&w).cloned())
                    .or_else(|| (tool == "launch_app").then(|| args.get("name").and_then(Value::as_str).unwrap_or("").to_string()))
                    .unwrap_or_default();
                let id = uuid::Uuid::new_v4().to_string();
                hub.desktop_event(
                    sid,
                    EventBody::ComputerUse {
                        action_id: id.clone(),
                        tool: tool.into(),
                        status: "running".into(),
                        message: label.clone(),
                        duration_ms: 0,
                        images: vec![],
                    },
                );
                let start = Instant::now();
                let response = self
                    .request_for(
                        "tools/call",
                        json!({"name":tool,"arguments":args}),
                        Some((sid, ticket)),
                        Some(hub),
                    )
                    .await;
                // No cancelled call is reported as successful, even if its response raced with revocation.
                let response = response.and_then(|r| self.grant(sid, ticket, true).map(|_| r));
                if let Ok(value) = &response {
                    let data = structured(value);
                    let mut control = self.control.lock().unwrap();
                    if let Some(current) = control.grants.get_mut(sid).filter(|g| g.ticket == ticket) {
                        remember(current, tool, window, &args, &data);
                    }
                }
                let (status, images) = match &response {
                    Ok(value) => (outcome(value), save_images(sid, value).unwrap_or_default()),
                    Err(_) => ("interrupted", vec![]),
                };
                let model_images = images.clone();
                hub.desktop_event(
                    sid,
                    EventBody::ComputerUse {
                        action_id: id,
                        tool: tool.into(),
                        status: status.into(),
                        message: label,
                        duration_ms: start.elapsed().as_millis() as u64,
                        images,
                    },
                );
                response.map(|value| {
                    let mut value = model_result(value);
                    // Kiro CLI v2 serializes MCP images as text. Its native file reader handles vision correctly.
                    if info.tool == yi_proto::ToolKind::Kiro {
                        if let Some(content) = value.get_mut("content").and_then(Value::as_array_mut) {
                            content.retain(|item| item["type"] != "image");
                            for image in &model_images {
                                let path = crate::paths::base_dir().join("uploads").join(sid).join(&image.id);
                                content.push(json!({"type":"text","text":format!("Yi Console captured the screenshot. Screenshot artifact: {}. Kiro CLI cannot view MCP image blocks directly: use your built-in read/fs_read image mode to open this exact image file before judging pixels. Read only this returned screenshot artifact, not other files. Do not infer screenshot contents from the accessibility tree.", path.display())}));
                            }
                        }
                    }
                    value
                })
            }
            _ => bail!("不支持的 Computer Use MCP 请求"),
        }
    }
}

/// Some CLIs expose only content to the model. Preserve structured action outcomes and element tokens there too.
fn model_result(mut value: Value) -> Value {
    if value["structuredContent"].is_object() {
        let text = value["structuredContent"].to_string();
        if text.len() <= 64 * 1024 {
            if let Some(content) = value.get_mut("content").and_then(Value::as_array_mut) {
                content.push(json!({"type":"text","text":text}));
            }
        }
    }
    value
}

fn outcome(value: &Value) -> &'static str {
    let data = structured(value);
    let effect = data["effect"]
        .as_str()
        .or_else(|| data["verification"]["effect"].as_str())
        .unwrap_or("");
    if value["isError"] == true || matches!(effect, "refused" | "suspected_noop") {
        "failed"
    } else if effect == "unverifiable" {
        "unverified"
    } else {
        "completed"
    }
}

fn allowed(name: &str) -> bool {
    READ_TOOLS.contains(&name) || WRITE_TOOLS.contains(&name)
}

/// Recheck after queueing: another read or a pause may have invalidated an input's snapshot.
fn validate_snapshot(args: &Value, grant: &Grant, tool: &str) -> Result<()> {
    let window = args["window_id"].as_u64();
    if let Some(token) = args["element_token"].as_str() {
        let known = match window {
            Some(w) => grant.tokens.get(&w).is_some_and(|t| t.contains(token)),
            None => grant.tokens.values().any(|t| t.contains(token)),
        };
        if !known {
            bail!("界面元素已失效，请先重新读取窗口");
        }
    }
    if WRITE_TOOLS.contains(&tool)
        && (args.get("x").is_some() || args.get("from_x").is_some())
        && grant.captures.is_empty()
    {
        bail!("坐标操作前请先读取窗口截图");
    }
    if let Some(capture) = args["capture_id"].as_str() {
        if !grant.captures.values().any(|c| c == capture) {
            bail!("界面截图已失效，请先重新读取窗口");
        }
    }
    Ok(())
}

/// What a successful call teaches the gateway: tokens and capture of the window it read, window titles, and the
/// window the last action went to.
fn remember(grant: &mut Grant, tool: &str, window: Option<u64>, args: &serde_json::Map<String, Value>, data: &Value) {
    if matches!(tool, "list_windows" | "launch_app" | "get_window_state" | "bring_to_front") {
        let mut found = vec![];
        collect_targets(data, None, &mut found);
        for t in found.into_iter().filter(|t| !t.title.is_empty()) {
            grant.titles.insert(t.window_id, t.title);
        }
    }
    match tool {
        "get_window_state" => {
            if let Some(w) = window {
                if let Some(elements) = data["elements"].as_array() {
                    grant.tokens.insert(w, elements.iter().filter_map(|e| e["element_token"].as_str().map(String::from)).collect());
                }
                match data["capture_id"].as_str() {
                    Some(c) => grant.captures.insert(w, c.to_string()),
                    None => grant.captures.remove(&w),
                };
            }
        }
        "get_desktop_state" => {
            if let Some(c) = data["capture_id"].as_str() {
                grant.captures.insert(0, c.to_string());
            }
        }
        _ => {}
    }
    if let (Some(w), Some(pid)) = (window, args.get("pid").and_then(Value::as_u64)) {
        let title = grant.titles.get(&w).cloned().unwrap_or_default();
        grant.last = Some(Target { pid, window_id: w, title });
    }
}

fn structured(value: &Value) -> Value {
    if value["structuredContent"].is_object() {
        return value["structuredContent"].clone();
    }
    for item in value["content"].as_array().into_iter().flatten() {
        if let Some(text) = item["text"].as_str() {
            if let Ok(v) = serde_json::from_str(text) {
                return v;
            }
        }
    }
    value.clone()
}

fn collect_targets(value: &Value, parent_pid: Option<u64>, found: &mut Vec<Target>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_targets(item, parent_pid, found);
            }
        }
        Value::Object(map) => {
            let pid = map.get("pid").and_then(Value::as_u64).or(parent_pid);
            let id = map
                .get("window_id")
                .or_else(|| map.get("windowId"))
                .or_else(|| map.get("id"))
                .and_then(Value::as_u64);
            let title = map
                .get("title")
                .or_else(|| map.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if let (Some(pid), Some(id)) = (pid, id) {
                if id > 0 && !title.is_empty() {
                    found.push(Target {
                        pid,
                        window_id: id,
                        title: title.into(),
                    });
                }
            }
            for child in map.values().filter(|v| v.is_object() || v.is_array()) {
                collect_targets(child, pid, found);
            }
        }
        _ => {}
    }
}

fn save_images(sid: &str, value: &Value) -> Result<Vec<ImageRef>> {
    let mut images = vec![];
    for item in value["content"].as_array().into_iter().flatten() {
        if item["type"] != "image" {
            continue;
        }
        let mime = item["mimeType"].as_str().unwrap_or("");
        if !matches!(mime, "image/png" | "image/jpeg" | "image/webp") {
            continue;
        }
        let data = item["data"].as_str().unwrap_or("");
        if data.len() > 12 * 1024 * 1024 || images.len() >= 2 {
            continue;
        }
        let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
        let id = format!(
            "desktop-{}.{}",
            uuid::Uuid::new_v4(),
            if mime == "image/png" {
                "png"
            } else if mime == "image/jpeg" {
                "jpg"
            } else {
                "webp"
            }
        );
        let path = crate::paths::base_dir().join("uploads").join(sid).join(&id);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, bytes)?;
        images.push(ImageRef {
            id,
            mime_type: mime.into(),
        });
    }
    prune_images(sid);
    Ok(images)
}

/// Keep only this feature's bounded checkpoint cache; user attachments are never removed here.
fn prune_images(sid: &str) {
    let dir = crate::paths::base_dir().join("uploads").join(sid);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("desktop-")
                || !matches!(
                    entry.path().extension().and_then(|s| s.to_str()),
                    Some("png" | "jpg" | "webp")
                )
            {
                return None;
            }
            let meta = entry.metadata().ok()?;
            meta.is_file()
                .then(|| (meta.modified().ok(), meta.len(), entry.path()))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    let mut bytes = 0u64;
    for (index, (_, size, path)) in files.into_iter().enumerate() {
        bytes = bytes.saturating_add(size);
        if index >= 24 || bytes > 80 * 1024 * 1024 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn driver_path() -> Result<PathBuf> {
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        use sha2::{Digest, Sha256};
        const BYTES: &[u8] =
            include_bytes!("../../../../runtime/computer-use/windows-x64/cua-driver.exe");
        const LICENSE: &[u8] =
            include_bytes!("../../../../runtime/computer-use/windows-x64/LICENSE.md");
        let dir = crate::paths::base_dir()
            .join("bin")
            .join(format!("computer-use-{VERSION}"));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("cua-driver.exe");
        let valid =
            std::fs::read(&path).is_ok_and(|b| format!("{:x}", Sha256::digest(b)) == BINARY_SHA);
        if !valid {
            if format!("{:x}", Sha256::digest(BYTES)) != BINARY_SHA {
                bail!("内置桌面驱动校验失败");
            }
            let temp = dir.join("cua-driver.tmp");
            std::fs::write(&temp, BYTES)?;
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            std::fs::rename(temp, &path)?;
        }
        std::fs::write(dir.join("LICENSE.md"), LICENSE)?;
        return Ok(path);
    }
    #[cfg(not(all(windows, target_arch = "x86_64")))]
    {
        let _ = BINARY_SHA;
        bail!("操作电脑首版仅支持 Windows x64");
    }
}

struct Peer {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next: u64,
}
impl Peer {
    async fn start() -> Result<Self> {
        let path = driver_path()?;
        let mut command = Command::new(path);
        command
            .args(["mcp", "--direct"])
            .env("CUA_DRIVER_EMBEDDED", "1")
            .env("CUA_DRIVER_RS_TELEMETRY_ENABLED", "0")
            .env("CUA_DRIVER_RS_UPDATE_CHECK", "0")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn()?;
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut peer = Self {
            child,
            input,
            output,
            next: 1,
        };
        peer.request("initialize", json!({"protocolVersion":"2025-06-18","clientInfo":{"name":"yi-console","version":"1"},"capabilities":{}})).await?;
        peer.input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await?;
        Ok(peer)
    }
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        let id = self.next;
        let mut bytes =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        bytes.push(b'\n');
        self.input.write_all(&bytes).await?;
        self.input.flush().await?;
        loop {
            let mut line = String::new();
            if self.output.read_line(&mut line).await? == 0 {
                bail!("桌面驱动连接已断开，请重新读取窗口");
            }
            if line.len() > 32 * 1024 * 1024 {
                bail!("桌面驱动结果过大");
            }
            let value: Value = serde_json::from_str(&line)?;
            if value["id"] != id {
                continue;
            }
            if !value["error"].is_null() {
                bail!(
                    "桌面驱动请求失败：{}",
                    value["error"]["message"].as_str().unwrap_or("unknown")
                );
            }
            return Ok(value["result"].clone());
        }
    }
}

/// Drop the arguments the gateway owns. Uses get_mut only: indexing a Value mutably inserts null for a missing
/// key, and a tool with `"required": null` makes strict clients (Qoder) reject the whole tool list.
fn hide_gateway_args(tool: &mut Value) {
    const HIDDEN: [&str; 2] = ["screenshot_out_file", "session"];
    let Some(schema) = tool.get_mut("inputSchema").and_then(Value::as_object_mut) else { return };
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        for key in HIDDEN {
            properties.remove(key);
        }
    }
    match schema.get_mut("required") {
        Some(Value::Array(required)) => required.retain(|v| !v.as_str().is_some_and(|k| HIDDEN.contains(&k))),
        Some(_) => {
            schema.remove("required");
        }
        None => {}
    }
}

#[derive(Debug, PartialEq)]
enum ProxyAccess {
    Allow,
    Bind,
    Deny,
}

/// bound: the proxy pid holding the ticket and its parent (the AI tool). A second proxy of the same tool is allowed
/// (some CLIs open the server twice); any other process only once the bound proxy has exited.
fn proxy_access(bound: Option<(u32, Option<u32>)>, pid: u32, parent: Option<u32>, alive: impl Fn(u32) -> bool) -> ProxyAccess {
    match bound {
        Some((b, _)) if b == pid => ProxyAccess::Allow,
        Some((_, Some(tool))) if parent == Some(tool) => ProxyAccess::Allow,
        Some((b, _)) if alive(b) => ProxyAccess::Deny,
        _ => ProxyAccess::Bind,
    }
}

/// The proxy must reach this daemon's pipe. --home switches to a hashed pipe name (see paths::pipe_name), so pass it
/// only when the daemon itself runs with YI_CONSOLE_HOME; otherwise the proxy would look for a different daemon.
fn proxy_args(home: Option<std::ffi::OsString>, sid: &str, ticket: &std::path::Path) -> Vec<String> {
    let mut args = vec![];
    if let Some(home) = home.filter(|v| !v.is_empty()) {
        args.extend(["--home".to_string(), home.to_string_lossy().into_owned()]);
    }
    args.extend(["computer-use-mcp", "--session", sid, "--ticket-file"].map(String::from));
    args.push(ticket.to_string_lossy().into_owned());
    args
}

pub fn proxy_main(args: &[String]) -> Result<()> {
    let arg = |key: &str| {
        args.iter()
            .position(|a| a == key)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let sid = arg("--session").ok_or_else(|| anyhow!("缺少会话标识"))?;
    let path = arg("--ticket-file").ok_or_else(|| anyhow!("缺少桌面连接凭据"))?;
    let value: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let ticket = value["ticket"]
        .as_str()
        .ok_or_else(|| anyhow!("桌面连接凭据无效"))?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.len() > 128 * 1024 {
            bail!("MCP 请求过大");
        }
        let packet: Value = serde_json::from_str(&line)?;
        if packet.get("id").is_none() {
            continue;
        }
        let id = packet["id"].clone();
        // Never start a daemon from here: a new one has no grant for this session
        let result = crate::daemon_call_running(
            yi_proto::methods::COMPUTER_USE_MCP,
            json!({"sessionId":sid,"ticket":ticket,"packet":packet}),
        );
        let response = match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(e) if packet["method"] == "tools/call" => {
                json!({"jsonrpc":"2.0","id":id,"result":{"isError":true,"content":[{"type":"text","text":e.to_string()}]}})
            }
            Err(e) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":e.to_string()}})
            }
        };
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}

/// The CLI keeps the original model result. Only the console's display copy drops screenshot bytes.
pub fn display_value(value: &Value) -> Value {
    fn redact(value: &mut Value) {
        match value {
            Value::Array(items) => {
                for item in items {
                    redact(item);
                }
            }
            Value::Object(map) => {
                if map.get("type").and_then(Value::as_str) == Some("image") {
                    map.remove("data");
                }
                for child in map.values_mut() {
                    redact(child);
                }
            }
            _ => {}
        }
    }
    let mut value = value.clone();
    redact(&mut value);
    value
}

pub fn display_result(value: &Value) -> String {
    crate::util::truncate(&display_value(value).to_string(), 64 * 1024)
}

mod process {
    #[cfg(windows)]
    pub fn alive(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        const STILL_ACTIVE: u32 = 259;
        // Safety: the handle is checked and closed here
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            let mut code = 0u32;
            let running = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE;
            CloseHandle(h);
            running
        }
    }

    #[cfg(windows)]
    pub fn parent(pid: u32) -> Option<u32> {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
        // Safety: the snapshot handle is checked and closed; the entry is sized as the API requires
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut found = None;
            let mut ok = Process32FirstW(snap, &mut entry) != 0;
            while ok {
                if entry.th32ProcessID == pid {
                    found = Some(entry.th32ParentProcessID).filter(|p| *p != 0);
                    break;
                }
                ok = Process32NextW(snap, &mut entry) != 0;
            }
            CloseHandle(snap);
            found
        }
    }

    // Computer use is Windows-only; elsewhere never treat a bound proxy as gone
    #[cfg(not(windows))]
    pub fn alive(_pid: u32) -> bool {
        true
    }

    #[cfg(not(windows))]
    pub fn parent(_pid: u32) -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_binds_to_the_tools_proxy() {
        let alive = |pid: u32| pid != 9;
        assert_eq!(proxy_access(None, 10, Some(1), alive), ProxyAccess::Bind);
        assert_eq!(proxy_access(Some((10, Some(1))), 10, Some(1), alive), ProxyAccess::Allow);
        // The model's own shell read the ticket and started a proxy: different parent, bound proxy still running
        assert_eq!(proxy_access(Some((10, Some(1))), 20, Some(5), alive), ProxyAccess::Deny);
        assert_eq!(proxy_access(Some((10, None)), 20, None, alive), ProxyAccess::Deny);
        // Same tool opening the server again
        assert_eq!(proxy_access(Some((10, Some(1))), 21, Some(1), alive), ProxyAccess::Allow);
        // The bound proxy exited (the tool restarted its MCP servers)
        assert_eq!(proxy_access(Some((9, Some(1))), 22, Some(3), alive), ProxyAccess::Bind);
    }

    #[test]
    fn hide_gateway_args_never_adds_null() {
        let mut no_required = json!({"name":"zoom","inputSchema":{"type":"object","properties":{"pid":{},"session":{}}}});
        hide_gateway_args(&mut no_required);
        assert!(no_required["inputSchema"].get("required").is_none(), "{no_required}");
        assert!(no_required["inputSchema"]["properties"].get("session").is_none());
        // The model picks the window now: pid and window_id stay
        assert!(no_required["inputSchema"]["properties"].get("pid").is_some());
        let mut bare = json!({"name":"verify_state"});
        hide_gateway_args(&mut bare);
        assert!(bare.get("inputSchema").is_none());
        let mut both = json!({"name":"click","inputSchema":{"type":"object","properties":{"session":{},"pid":{}},"required":["session","pid"]}});
        hide_gateway_args(&mut both);
        assert_eq!(both["inputSchema"]["required"], json!(["pid"]));
        let mut null = json!({"name":"x","inputSchema":{"type":"object","required":null}});
        hide_gateway_args(&mut null);
        assert!(null["inputSchema"].get("required").is_none());
    }

    #[test]
    fn proxy_args_follow_daemon_home() {
        // An installed daemon has no YI_CONSOLE_HOME: the proxy must not get --home, or it looks for a hashed pipe
        let ticket = std::path::Path::new("t.json");
        assert_eq!(proxy_args(None, "s1", ticket), ["computer-use-mcp", "--session", "s1", "--ticket-file", "t.json"]);
        assert_eq!(proxy_args(Some("".into()), "s1", ticket)[0], "computer-use-mcp");
        assert_eq!(&proxy_args(Some("/tmp/yi-iso".into()), "s1", ticket)[..3], ["--home", "/tmp/yi-iso", "computer-use-mcp"]);
    }
    fn target() -> Target {
        Target {
            pid: 42,
            window_id: 77,
            title: "Validation".into(),
        }
    }
    fn grant() -> Grant {
        Grant {
            ticket: "valid".into(),
            paused: false,
            tokens: HashMap::new(),
            captures: HashMap::new(),
            titles: HashMap::new(),
            last: None,
            proxy: None,
        }
    }
    #[test]
    fn desktop_tools_cover_apps_but_not_driver_management() {
        for tool in ["list_windows", "launch_app", "hotkey", "get_desktop_state", "double_click", "clipboard_write"] {
            assert!(allowed(tool), "{tool}");
        }
        for tool in ["kill_app", "set_config", "install_extension", "replay_trajectory", "page", "browser_navigate", "start_session"] {
            assert!(!allowed(tool), "{tool}");
        }
    }
    #[test]
    fn reads_are_remembered_per_window() {
        let mut g = grant();
        let args = |w: u64| json!({"pid":42,"window_id":w}).as_object().unwrap().clone();
        remember(&mut g, "list_windows", None, &Default::default(), &json!({"windows":[{"pid":42,"window_id":77,"title":"Validation"}]}));
        remember(&mut g, "get_window_state", Some(77), &args(77), &json!({"elements":[{"element_token":"s1:0"}],"capture_id":"c77"}));
        remember(&mut g, "get_window_state", Some(78), &args(78), &json!({"elements":[{"element_token":"s2:0"}],"capture_id":"c78"}));
        assert_eq!(g.last, Some(Target { pid: 42, window_id: 78, title: String::new() }));
        remember(&mut g, "click", Some(77), &args(77), &json!({}));
        assert_eq!(g.last, Some(target()));
        // A token from one window is not valid for another
        assert!(validate_snapshot(&json!({"window_id":77,"element_token":"s1:0"}), &g, "click").is_ok());
        assert!(validate_snapshot(&json!({"window_id":77,"element_token":"s2:0"}), &g, "click").is_err());
        assert!(validate_snapshot(&json!({"window_id":78,"capture_id":"c77","x":1,"y":1}), &g, "click").is_ok());
        assert!(validate_snapshot(&json!({"capture_id":"old","x":1,"y":1}), &g, "click").is_err());
    }
    #[test]
    fn refused_and_noop_are_not_success_and_display_copy_has_no_image_bytes() {
        assert_eq!(
            outcome(&json!({"structuredContent":{"effect":"suspected_noop"}})),
            "failed"
        );
        assert_eq!(
            outcome(&json!({"structuredContent":{"effect":"unverifiable"}})),
            "unverified"
        );
        let result = json!({"content":[{"type":"image","data":"PRIVATE_IMAGE_BYTES","mimeType":"image/png"},{"type":"text","text":"result"}]});
        let display = display_result(&result);
        assert!(!display.contains("PRIVATE_IMAGE_BYTES"));
        assert!(display.contains("result"));
        assert_eq!(result["content"][0]["data"], "PRIVATE_IMAGE_BYTES");
    }
    #[tokio::test]
    async fn queued_proxy_cannot_execute_after_stop_or_use_another_sessions_ticket() {
        let desktop = std::sync::Arc::new(Desktop::new());
        let sid = uuid::Uuid::new_v4().to_string();
        {
            let mut control = desktop.control.lock().unwrap();
            control.owner = Some(sid.clone());
            control.grants.insert(
                sid.clone(),
                grant(),
            );
        }
        assert!(desktop.grant(&sid, "wrong", true).is_err());
        let driver = desktop.driver.lock().await;
        let queued = desktop.clone();
        let queued_sid = sid.clone();
        let task = tokio::spawn(async move {
            queued
                .request_for("tools/call", json!({}), Some((&queued_sid, "valid")), None)
                .await
        });
        tokio::task::yield_now().await;
        desktop.change(&sid, "stop").unwrap();
        drop(driver);
        let error = task.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("已关闭"), "{error}");
        assert!(desktop.control.lock().unwrap().owner.is_none());
    }
    #[test]
    fn stopping_an_unrelated_session_does_not_cancel_the_desktop_owner() {
        let desktop = Desktop::new();
        let change = desktop.cancel.subscribe();
        desktop.control.lock().unwrap().owner = Some("active".into());
        desktop
            .change(&uuid::Uuid::new_v4().to_string(), "stop")
            .unwrap();
        assert!(!change.has_changed().unwrap());
        assert_eq!(
            desktop.control.lock().unwrap().owner.as_deref(),
            Some("active")
        );
    }
    #[test]
    fn next_message_resumes_a_paused_grant_unless_the_desktop_was_taken() {
        let desktop = Desktop::new();
        let grant = || Grant { ticket: "t".into(), paused: false, tokens: HashMap::new(), captures: HashMap::new(), titles: HashMap::new(), last: None, proxy: None };
        desktop.control.lock().unwrap().grants.insert("a".into(), grant());
        desktop.control.lock().unwrap().owner = Some("a".into());
        desktop.change("a", "pause").unwrap();
        desktop.resume_if_paused("a");
        assert!(desktop.authorizes("a", "mcp__yi_computer_use__click"));
        desktop.change("a", "pause").unwrap();
        desktop.control.lock().unwrap().owner = Some("b".into());
        desktop.resume_if_paused("a");
        assert!(!desktop.authorizes("a", "mcp__yi_computer_use__click"));
        desktop.resume_if_paused("none");
    }
    #[test]
    fn windows_parser_does_not_treat_arbitrary_nested_metadata_as_targets() {
        let mut found = vec![];
        collect_targets(
            &json!({"windows":[{"pid":42,"window_id":77,"title":"Validation"},{"title":"not a window"}]}),
            None,
            &mut found,
        );
        assert_eq!(found, vec![target()]);
    }
    #[test]
    fn input_requires_current_elements_and_coordinate_capture() {
        let mut g = grant();
        g.tokens.insert(77, HashSet::from(["s00000001:0".into()]));
        g.captures.insert(77, "current".into());
        assert!(validate_snapshot(&json!({"element_token":"s00000001:0"}), &g, "click").is_ok());
        assert!(validate_snapshot(&json!({"element_token":"s00000000:0"}), &g, "click").is_err());
        assert!(validate_snapshot(&json!({"x":10,"y":20,"capture_id":"old"}), &g, "click").is_err());
        let empty = grant();
        assert!(validate_snapshot(&json!({"from_x":10,"from_y":20,"to_x":30,"to_y":40}), &empty, "drag").is_err());
    }
}

/// A GUI runtime must run in a real interactive session, never the SSH/service desktop.
#[cfg(windows)]
fn desktop_available() -> bool {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcessId() -> u32;
        fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
    }
    #[link(name = "user32")]
    extern "system" {
        fn OpenInputDesktop(flags: u32, inherit: i32, access: u32) -> *mut c_void;
        fn GetUserObjectInformationW(
            object: *mut c_void,
            index: i32,
            info: *mut c_void,
            size: u32,
            needed: *mut u32,
        ) -> i32;
        fn CloseDesktop(desktop: *mut c_void) -> i32;
    }
    unsafe {
        let mut session = 0;
        if ProcessIdToSessionId(GetCurrentProcessId(), &mut session) == 0 || session == 0 {
            return false;
        }
        let desktop = OpenInputDesktop(0, 0, 1);
        if desktop.is_null() {
            return false;
        }
        let mut name = [0u16; 128];
        let mut needed = 0;
        let ok = GetUserObjectInformationW(
            desktop,
            2,
            name.as_mut_ptr().cast(),
            (name.len() * 2) as u32,
            &mut needed,
        );
        CloseDesktop(desktop);
        ok != 0
            && String::from_utf16_lossy(
                &name[..name.iter().position(|c| *c == 0).unwrap_or(name.len())],
            )
            .eq_ignore_ascii_case("default")
    }
}
#[cfg(not(windows))]
fn desktop_available() -> bool {
    false
}

pub fn model_hint(extra_args: &[String]) -> &'static str {
    if extra_args.iter().any(|arg| arg.contains("yi_computer_use")) {
        concat!(" For GUI tasks on this computer's desktop, call the yi_computer_use MCP tools directly. ", guide!())
    } else {
        ""
    }
}

/// ACP tools take no system prompt from us, and a resumed session may still remember turns from before computer use
/// was on (or from when its tools failed to load). Sent with the first prompt after each launch that has the server.
pub fn acp_notice() -> &'static str {
    concat!(
        "<system-reminder>Yi Console turned on computer use for this session. The yi_computer_use MCP tools ",
        "(list_windows, launch_app, get_window_state, click, type_text, press_key, hotkey and more) ",
        "are in your tool list now; earlier messages saying they are missing or broken are outdated. Use them only when the task needs the desktop. ",
        guide!(),
        "</system-reminder>"
    )
}
