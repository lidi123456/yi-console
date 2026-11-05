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
const READ_TOOLS: &[&str] = &["get_window_state", "verify_state", "zoom"];
const WRITE_TOOLS: &[&str] = &[
    "click",
    "type_text",
    "set_value",
    "press_key",
    "scroll",
    "drag",
    "move_cursor",
    "invoke_menu",
];
const GUIDE: &str = "Operate only the window selected by the user in Yi Console. Read get_window_state before input. Use element_token from the latest snapshot; do not invent tokens or infer coordinates from resized images. Verify the requested result after input. A refused, suspected_noop, cancelled or unverifiable action is not proof of success. Do not retry an uncertain action blindly. Pause/stop/busy errors require returning control to the user. Use existing API, filesystem and browser tools for tasks they can complete directly.";

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
    target: Target,
    paused: bool,
    tokens: HashSet<String>,
    capture: Option<String>,
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
            "ownerSessionId": control.owner, "target": grant.map(|g| &g.target),
        })
    }

    fn ticket_path(sid: &str) -> PathBuf {
        crate::paths::base_dir()
            .join("computer-use")
            .join(format!("{sid}.json"))
    }

    pub fn configure(&self, sid: &str, target: Target) -> Result<()> {
        if !cfg!(all(windows, target_arch = "x86_64")) {
            bail!("操作电脑首版仅支持 Windows x64");
        }
        if !desktop_available() {
            bail!("桌面不可用，请在已登录且解锁的 Windows 用户会话中运行 Yi Console");
        }
        if target.pid == 0 || target.window_id == 0 {
            bail!("请选择有效的目标窗口");
        }
        let mut control = self.control.lock().unwrap();
        if control.owner.as_deref().is_some_and(|owner| owner != sid) {
            bail!("桌面正在被其他会话使用，请先暂停或停止该会话");
        }
        // Replacing a grant invalidates old proxies even when their CLI has not exited yet.
        let grant = Grant {
            ticket: uuid::Uuid::new_v4().to_string(),
            target,
            paused: false,
            tokens: HashSet::new(),
            capture: None,
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
                grant.capture = None;
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

    /// Selecting the window is the user's authorization; avoid asking again for every MCP click.
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
        Some(
            json!({"command": std::env::current_exe().ok()?.to_string_lossy(),
            "args": ["--home", crate::paths::base_dir().to_string_lossy().as_ref(), "computer-use-mcp", "--session", sid,
                "--ticket-file", Self::ticket_path(sid).to_string_lossy().as_ref()] }),
        )
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
        let result = self.mcp(hub, sid, &ticket, json!({"method":"tools/call","params":{"name":"get_window_state","arguments":{"include_screenshot":true}}})).await?;
        if result["isError"] == true {
            bail!("无法读取目标窗口，请刷新后重试");
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
                "serverInfo":{"name":SERVER,"version":"1"},"capabilities":{"tools":{},"resources":{}},"instructions":format!("{GUIDE} Selected window: pid={}, window_id={}, title={}", grant.target.pid, grant.target.window_id, grant.target.title)}),
            ),
            "tools/list" => {
                let mut response = self.request("tools/list", json!({})).await?;
                if let Some(tools) = response["tools"].as_array_mut() {
                    tools.retain(|tool| tool["name"].as_str().is_some_and(allowed));
                    for tool in tools {
                        let hidden = [
                            "pid",
                            "window_id",
                            "scope",
                            "target",
                            "screenshot_out_file",
                            "session",
                        ];
                        let description = tool["description"].as_str().unwrap_or("");
                        tool["description"] = json!(format!("Yi Console automatically supplies the user's authorized window. Do not supply pid, window_id, scope or target. {description}"));
                        if let Some(properties) = tool["inputSchema"]["properties"].as_object_mut()
                        {
                            for key in hidden {
                                properties.remove(key);
                            }
                        }
                        if let Some(required) = tool["inputSchema"]["required"].as_array_mut() {
                            required.retain(|value| {
                                !value.as_str().is_some_and(|key| hidden.contains(&key))
                            });
                        }
                    }
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
                enforce_target(&mut args, &grant.target)?;
                if args.contains_key("screenshot_out_file") {
                    bail!("截图只能保存在当前会话的产物目录");
                }
                validate_snapshot(&json!(args), &grant, tool)?;
                let id = uuid::Uuid::new_v4().to_string();
                hub.desktop_event(
                    sid,
                    EventBody::ComputerUse {
                        action_id: id.clone(),
                        tool: tool.into(),
                        status: "running".into(),
                        message: grant.target.title.clone(),
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
                if tool == "get_window_state" {
                    if let Ok(value) = &response {
                        let snapshot = structured(value);
                        let mut control = self.control.lock().unwrap();
                        if let Some(current) =
                            control.grants.get_mut(sid).filter(|g| g.ticket == ticket)
                        {
                            if let Some(elements) = snapshot["elements"].as_array() {
                                current.tokens = elements
                                    .iter()
                                    .filter_map(|e| e["element_token"].as_str().map(String::from))
                                    .collect();
                            }
                            current.capture = snapshot["capture_id"].as_str().map(String::from);
                        }
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
                        message: grant.target.title,
                        duration_ms: start.elapsed().as_millis() as u64,
                        images,
                    },
                );
                response.map(|value| {
                    let mut value = model_result(value);
                    // Kiro CLI v2 serializes MCP images as text. Its native file reader handles vision correctly.
                    if info.tool == yi_proto::ToolKind::Kiro {
                        if let Some(content) = value["content"].as_array_mut() {
                            content.retain(|item| item["type"] != "image");
                            for image in &model_images {
                                let path = crate::paths::base_dir().join("uploads").join(sid).join(&image.id);
                                content.push(json!({"type":"text","text":format!("Yi Console captured the authorized window. Screenshot artifact: {}. Kiro CLI cannot view MCP image blocks directly: use your built-in read/fs_read image mode to open this exact image file before judging pixels. Read only this returned screenshot artifact, not other files. Do not infer screenshot contents from the accessibility tree.", path.display())}));
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
            if let Some(content) = value["content"].as_array_mut() {
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
    if let Some(token) = args["element_token"].as_str() {
        if !grant.tokens.contains(token) {
            bail!("界面元素已失效或不属于当前授权窗口，请先重新读取窗口");
        }
    }
    if WRITE_TOOLS.contains(&tool)
        && (args.get("x").is_some() || args.get("from_x").is_some())
        && grant.capture.is_none()
    {
        bail!("坐标操作前请先读取当前目标窗口截图");
    }
    if let Some(capture) = args["capture_id"].as_str() {
        if grant.capture.as_deref() != Some(capture) {
            bail!("界面截图已失效，请先重新读取窗口");
        }
    }
    Ok(())
}

fn enforce_target(args: &mut serde_json::Map<String, Value>, target: &Target) -> Result<()> {
    if args
        .get("pid")
        .is_some_and(|p| p.as_u64() != Some(target.pid))
        || args
            .get("window_id")
            .is_some_and(|w| w.as_u64() != Some(target.window_id))
    {
        bail!("操作目标与用户选择的窗口不一致");
    }
    if args.get("scope").is_some_and(|v| v != "window") {
        bail!("当前会话仅授权指定窗口");
    }
    if let Some(t) = args.get("target") {
        if t["kind"] != "window"
            || t["pid"].as_u64() != Some(target.pid)
            || t["window_id"].as_u64() != Some(target.window_id)
        {
            bail!("操作目标与用户选择的窗口不一致");
        }
    }
    // An agent cannot attach its request to a different lifecycle session.
    args.remove("session");
    args.insert("pid".into(), json!(target.pid));
    args.insert("window_id".into(), json!(target.window_id));
    Ok(())
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
        let result = crate::daemon_call(
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

#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> Target {
        Target {
            pid: 42,
            window_id: 77,
            title: "Validation".into(),
        }
    }
    #[test]
    fn target_restriction_rejects_nested_desktop_and_foreign_windows() {
        for args in [
            json!({"pid":41}),
            json!({"window_id":78}),
            json!({"scope":"desktop"}),
            json!({"target":{"kind":"desktop","display_id":"primary"}}),
            json!({"target":{"kind":"window","pid":41,"window_id":77}}),
        ] {
            let mut args = args.as_object().unwrap().clone();
            assert!(enforce_target(&mut args, &target()).is_err());
        }
        let mut args = json!({"text":"test","session":"foreign"})
            .as_object()
            .unwrap()
            .clone();
        enforce_target(&mut args, &target()).unwrap();
        assert_eq!(args["pid"], 42);
        assert_eq!(args["window_id"], 77);
        assert!(!args.contains_key("session"));
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
                Grant {
                    ticket: "valid".into(),
                    target: target(),
                    paused: false,
                    tokens: HashSet::new(),
                    capture: None,
                },
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
        let grant = Grant {
            ticket: "t".into(),
            target: target(),
            paused: false,
            tokens: HashSet::from(["s00000001:0".into()]),
            capture: Some("current".into()),
        };
        assert!(
            validate_snapshot(&json!({"element_token":"s00000001:0"}), &grant, "click").is_ok()
        );
        assert!(
            validate_snapshot(&json!({"element_token":"s00000000:0"}), &grant, "click").is_err()
        );
        assert!(
            validate_snapshot(&json!({"x":10,"y":20,"capture_id":"old"}), &grant, "click").is_err()
        );
        let mut empty = grant.clone();
        empty.capture = None;
        assert!(validate_snapshot(
            &json!({"from_x":10,"from_y":20,"to_x":30,"to_y":40}),
            &empty,
            "drag"
        )
        .is_err());
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
        " For tasks in the desktop window authorized by the user, use the yi_computer_use MCP tools. Its initialize instructions name the exact target. Read the window first, act through its current element tokens and verify the result. Do not bypass paused, stopped, busy or denied desktop tools through shell scripts or other desktop-control tools. A tool returning is not proof that the requested UI change happened."
    } else {
        ""
    }
}
