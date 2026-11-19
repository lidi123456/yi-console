//! Built-in plugins (Rust, same interfaces as installable ones): `cron`, `session-events`, `app-notify`.

use super::cron::Cron;
use super::{Manifest, PluginHost, PluginInstance, PluginRuntime, SendResult, Sender, Text, TriggerEvent, API_VERSION};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const CRON: &str = "cron";
pub const SESSION_EVENTS: &str = "session-events";
pub const APP_NOTIFY: &str = "app-notify";
/// The single instance of `app-notify`, referenced in a Bot's `notify` list
pub const APP_INSTANCE: &str = "app";
/// Ids installable plugins may not take
pub const RESERVED: &[&str] = &[CRON, SESSION_EVENTS, APP_NOTIFY, "app", "builtin"];

fn text(zh: &str, en: &str) -> Text {
    Text { zh: zh.into(), en: en.into() }
}

fn manifest(id: &str, name: Text, description: Text, caps: &[&str], config: Value, prompt: Option<&str>) -> Manifest {
    Manifest {
        id: id.into(),
        version: env!("CARGO_PKG_VERSION").into(),
        api: API_VERSION.into(),
        runtime: "builtin".into(),
        main: None,
        name,
        description,
        capabilities: caps.iter().map(|s| s.to_string()).collect(),
        permissions: Default::default(),
        config,
        prompt: prompt.map(str::to_string),
        singleton: false,
        bundled: false,
    }
}

/// Shows a system notification on the desktop (the frontend's notify preferences apply)
pub type AppNotify = Arc<dyn Fn(&str, &str) + Send + Sync>;

pub struct Builtin {
    pub app_notify: AppNotify,
}

impl PluginRuntime for Builtin {
    fn kind(&self) -> &'static str {
        "builtin"
    }

    fn manifests(&self) -> Vec<Manifest> {
        let mut app = manifest(
            APP_NOTIFY,
            text("App 通知", "App notification"),
            text("在这台电脑上弹出系统通知", "System notifications on this computer"),
            &["notify"],
            json!({"type": "object", "properties": {}}),
            None,
        );
        app.singleton = true;
        vec![
            manifest(
                CRON,
                text("定时", "Schedule"),
                text("按 cron 表达式定时触发，带一段固定的说明。App 没运行时错过的时间点直接跳过", "Fires on a cron schedule with a fixed instruction. Times missed while the app was not running are skipped"),
                &["trigger"],
                json!({
                    "type": "object",
                    "required": ["schedule", "text"],
                    "properties": {
                        "schedule": {"type": "string", "title": "cron", "description": "分 时 日 月 周，例如 0 9 * * 1-5", "x-ui": "cron", "x-en": {"title": "cron", "description": "minute hour day month weekday, e.g. 0 9 * * 1-5"}},
                        "text": {"type": "string", "title": "说明", "x-ui": "textarea", "x-en": {"title": "Instruction"}},
                    },
                }),
                Some("Scheduled events: the text is the owner's standing instruction for this time slot; treat it as an explicit request."),
            ),
            manifest(
                SESSION_EVENTS,
                text("会话动态", "Session events"),
                text("其他会话结束一轮、出错或等待审批时触发（不含 Bot 自己的会话和任务）", "Fires when another session finishes a turn, fails or waits for approval (not the Bots' own sessions and tasks)"),
                &["trigger"],
                json!({
                    "type": "object",
                    "properties": {
                        "host": {"type": "string", "title": "主机", "description": "留空表示所有已连接的主机", "x-ui": "host", "x-en": {"title": "Host", "description": "Empty means every connected host"}},
                        "match": {"type": "string", "title": "标题包含", "description": "只看标题包含这段文字的会话", "x-en": {"title": "Title contains", "description": "Only sessions whose title contains this text"}},
                        "on": {"type": "array", "title": "触发时机", "items": {"enum": ["turnEnd", "error", "pending"]}, "default": ["error", "pending"], "x-ui": "checks", "x-en": {"title": "When"}, "x-labels": {"zh": ["一轮结束", "出错", "等待审批"], "en": ["Turn ends", "Error", "Waiting for approval"]}},
                    },
                }),
                Some("Session events report what happened in other sessions. Look at the session only if the owner's sections ask for it; otherwise ignore."),
            ),
            app,
        ]
    }

    fn start(&self, plugin_id: &str, host: Arc<dyn PluginHost>) -> Result<Arc<dyn PluginInstance>> {
        match plugin_id {
            CRON => {
                let config = host.config();
                let cron = Cron::parse(config["schedule"].as_str().unwrap_or(""))?;
                let text = config["text"].as_str().unwrap_or("").to_string();
                let h = host.clone();
                let task = tokio::spawn(async move {
                    h.health("ok", "");
                    loop {
                        let Some(next) = cron.next(chrono::Local::now()) else {
                            h.health("error", "没有下一次触发时间");
                            return;
                        };
                        let wait = (next - chrono::Local::now()).to_std().unwrap_or_default();
                        tokio::time::sleep(wait).await;
                        // Woke up much later (the computer slept): that time was missed, skip it
                        let late = chrono::Local::now() - next;
                        if late > chrono::Duration::minutes(2) {
                            continue;
                        }
                        let ts = next.timestamp_millis();
                        h.emit(TriggerEvent {
                            key: ts.to_string(),
                            thread: format!("cron:{}:{ts}", h.instance_id()),
                            sender: Sender { id: "cron".into(), name: Some("cron".into()), bot: false },
                            addressed: true,
                            text: text.clone(),
                            url: None,
                            at: ts,
                            meta: json!({"scheduledAt": next.to_rfc3339()}),
                        });
                    }
                });
                Ok(Arc::new(CronInstance { task }))
            }
            SESSION_EVENTS => {
                host.health("ok", "");
                Ok(Arc::new(SessionEvents { host, last: Mutex::new(HashMap::new()) }))
            }
            APP_NOTIFY => Ok(Arc::new(AppNotifyInstance { notify: self.app_notify.clone() })),
            _ => bail!("未知的内置插件: {plugin_id}"),
        }
    }
}

struct CronInstance {
    task: tokio::task::JoinHandle<()>,
}

impl PluginInstance for CronInstance {
    fn stop(&self) {
        self.task.abort();
    }
}

struct AppNotifyInstance {
    notify: AppNotify,
}

impl PluginInstance for AppNotifyInstance {
    fn stop(&self) {}
    fn notify(&self, text: &str, _key: &str) -> Result<SendResult> {
        let (title, body) = text.split_once('\n').unwrap_or(("Yi Console", text));
        (self.notify)(title, body);
        Ok(SendResult::Sent)
    }
}

struct SessionEvents {
    host: Arc<dyn PluginHost>,
    /// Last seen (status, has pending) per session, to report transitions only
    last: Mutex<HashMap<String, (String, bool)>>,
}

impl PluginInstance for SessionEvents {
    fn stop(&self) {}

    fn on_host_event(&self, host: &str, method: &str, params: &Value) {
        if method != "session.updated" {
            return;
        }
        // Never the Bots' own sessions: a Bot watching itself would loop
        if !params["bot"].is_null() || !params["parentId"].is_null() {
            return;
        }
        let config = self.host.config();
        if let Some(h) = config["host"].as_str().filter(|h| !h.is_empty()) {
            if h != host {
                return;
            }
        }
        let title = params["title"].as_str().unwrap_or("");
        if let Some(m) = config["match"].as_str().filter(|m| !m.is_empty()) {
            if !title.contains(m) {
                return;
            }
        }
        let on: Vec<&str> = match config["on"].as_array() {
            Some(a) => a.iter().filter_map(|v| v.as_str()).collect(),
            None => vec!["error", "pending"],
        };
        let Some(sid) = params["id"].as_str() else { return };
        let status = params["status"].as_str().unwrap_or("").to_string();
        let pending = !params["pending"].is_null();
        let key = format!("{host}/{sid}");
        let prev = self.last.lock().unwrap().insert(key, (status.clone(), pending));
        // The first sighting only records the state
        let Some((prev_status, prev_pending)) = prev else { return };
        let busy = |s: &str| s == "running" || s == "waiting-permission";
        let kind = if status == "error" && prev_status != "error" {
            "error"
        } else if pending && !prev_pending {
            "pending"
        } else if busy(&prev_status) && status == "idle" {
            "turnEnd"
        } else {
            return;
        };
        if !on.contains(&kind) {
            return;
        }
        let name = if title.is_empty() { sid } else { title };
        let what = match kind {
            "error" => "failed",
            "pending" => "is waiting for approval or an answer",
            _ => "finished a turn",
        };
        let error_at = if kind == "error" {
            params["lastProcessErrorAt"].as_i64()
        } else {
            None
        };
        let at = error_at
            .or_else(|| params["updatedAt"].as_i64())
            .unwrap_or_else(|| chrono::Local::now().timestamp_millis());
        self.host.emit(TriggerEvent {
            key: format!("{host}/{sid}/{kind}/{at}"),
            thread: format!("session:{host}/{sid}"),
            sender: Sender { id: "session-events".into(), name: None, bot: false },
            addressed: false,
            text: format!("Session \"{name}\" ({}) on host {host} {what}.", params["tool"].as_str().unwrap_or("")),
            url: None,
            at,
            meta: json!({"host": host, "sessionId": sid, "kind": kind}),
        });
    }
}

/// The schema of a built-in plugin, for validation before an instance starts
pub fn schema(manifests: &[Manifest], plugin_id: &str) -> Result<Value> {
    manifests
        .iter()
        .find(|m| m.id == plugin_id)
        .map(|m| m.config.clone())
        .ok_or_else(|| anyhow!("插件不存在: {plugin_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Host {
        config: Value,
        events: Mutex<Vec<TriggerEvent>>,
    }

    impl PluginHost for Host {
        fn instance_id(&self) -> &str {
            "i1"
        }
        fn config(&self) -> Value {
            self.config.clone()
        }
        fn emit(&self, event: TriggerEvent) {
            self.events.lock().unwrap().push(event);
        }
        fn kv_get(&self, _key: &str) -> Option<String> {
            None
        }
        fn kv_set(&self, _key: &str, _value: Option<&str>) {}
        fn log(&self, _level: &str, _message: &str) {}
        fn health(&self, _state: &str, _detail: &str) {}
    }

    #[test]
    fn session_events_report_transitions_of_other_sessions_only() {
        let host = Arc::new(Host { config: json!({"on": ["turnEnd", "pending"]}), ..Default::default() });
        let p = SessionEvents { host: host.clone(), last: Mutex::new(HashMap::new()) };
        let upd = |status: &str, pending: bool, bot: bool| {
            json!({"id": "s1", "title": "Build", "tool": "claude", "status": status, "updatedAt": 1,
                   "pending": if pending { json!({"title": "x"}) } else { Value::Null },
                   "bot": if bot { json!({"botId": "b"}) } else { Value::Null }})
        };
        p.on_host_event("local", "session.updated", &upd("running", false, false));
        p.on_host_event("local", "session.updated", &upd("waiting-permission", true, false));
        p.on_host_event("local", "session.updated", &upd("idle", false, false));
        p.on_host_event("local", "session.updated", &upd("error", false, false));
        p.on_host_event("local", "session.updated", &upd("running", false, true));
        let kinds: Vec<String> = host.events.lock().unwrap().iter().map(|e| e.meta["kind"].as_str().unwrap().to_string()).collect();
        // error is not in "on"; the Bot's own session is never reported
        assert_eq!(kinds, ["pending", "turnEnd"]);
    }

    #[test]
    fn session_error_events_use_the_process_exit_time() {
        let host = Arc::new(Host { config: json!({"on": ["error"]}), ..Default::default() });
        let p = SessionEvents { host: host.clone(), last: Mutex::new(HashMap::new()) };
        let mut update = json!({"id": "s1", "status": "running", "updatedAt": 1});
        p.on_host_event("local", "session.updated", &update);
        update["status"] = json!("error");
        update["lastProcessErrorAt"] = json!(2);
        p.on_host_event("local", "session.updated", &update);

        let events = host.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].at, 2);
        assert_eq!(events[0].key, "local/s1/error/2");
    }
}
