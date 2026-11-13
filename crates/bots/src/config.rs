//! Bot configuration (stored as JSON in `bots.config`), with defaults and validation.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Tools a Bot or task may use: the AI tools with MCP support, chat mode only
pub const AI_TOOLS: [&str; 7] = ["claude", "codex", "cursor", "kiro", "qoder", "codebuddy", "antigravity"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BotConfig {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    /// Where tasks run; the first one is the default
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    /// The Bot's own session
    pub bot: SessionSpec,
    #[serde(default)]
    pub task: TaskDefaults,
    /// Plugin instance ids whose events this Bot handles
    #[serde(default)]
    pub triggers: Vec<String>,
    /// Where reminders go: "app" plus plugin instance ids with the notify capability
    #[serde(default = "default_notify")]
    pub notify: Vec<String>,
    #[serde(default)]
    pub mcp: McpChoice,
    #[serde(default)]
    pub accept: Accept,
    /// Plugin events neither addressed to the Bot nor from an allow-listed sender
    #[serde(default)]
    pub ambient: Ambient,
    /// Who may make explicit requests, per plugin instance
    #[serde(default)]
    pub allow: Vec<Allow>,
    #[serde(default)]
    pub budget: Budget,
    /// Scheduled instructions the Bot runs on its own
    #[serde(default)]
    pub routines: Vec<Routine>,
    /// Outgoing actions that wait for the owner (auto mode, turns the owner didn't start)
    #[serde(default)]
    pub approval: Approval,
    /// Plugin events outside these hours wait until they start; None: always
    #[serde(default)]
    pub active_hours: Option<ActiveHours>,
    /// User layer of the prompt
    #[serde(default)]
    pub prompt: PromptSections,
    #[serde(default)]
    pub prompt_revision: u32,
    /// Compact the Bot session when its context use passes this percentage
    #[serde(default = "default_compact_at")]
    pub compact_at: u32,
}

fn default_notify() -> Vec<String> {
    vec!["app".into()]
}

fn default_compact_at() -> u32 {
    70
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub host: String,
    pub cwd: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub about: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSpec {
    pub host: String,
    /// Empty: `~/.yi-console/bots/<bot id>` on that host
    #[serde(default)]
    pub cwd: String,
    pub tool: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskDefaults {
    /// Default tool; `model` and `effort` apply to it only
    #[serde(default = "default_task_tool")]
    pub tool: String,
    /// Other tools the Bot may pick per task (`spawn_task.tool`), following the owner's prompt sections
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default = "default_task_mode")]
    pub permission_mode: String,
    /// Allow tasks to operate the host's desktop GUI when `spawn_task` passes `computer_use: true`
    #[serde(default)]
    pub computer_use: bool,
    #[serde(default = "default_max")]
    pub max_concurrent: u32,
    /// 0: no limit
    #[serde(default = "default_timeout")]
    pub timeout_minutes: u32,
    /// Days an ended task keeps its session (and a worktree with uncommitted changes); 0: forever
    #[serde(default = "default_keep_days")]
    pub keep_days: u32,
}

fn default_task_tool() -> String {
    "claude".into()
}
fn default_task_mode() -> String {
    "bypass".into()
}
fn yes() -> bool {
    true
}
fn default_max() -> u32 {
    3
}
fn default_timeout() -> u32 {
    120
}
fn default_keep_days() -> u32 {
    7
}

impl Default for TaskDefaults {
    fn default() -> Self {
        TaskDefaults {
            tool: default_task_tool(),
            tools: vec![],
            model: None,
            effort: None,
            permission_mode: default_task_mode(),
            computer_use: false,
            max_concurrent: default_max(),
            timeout_minutes: default_timeout(),
            keep_days: default_keep_days(),
        }
    }
}

/// Extra MCP servers (by name, from each host's own MCP list) for the Bot session and for tasks.
/// Host MCP servers are assigned per tool in the host's settings; this list is informational for now
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpChoice {
    #[serde(default)]
    pub bot: Vec<String>,
    #[serde(default)]
    pub task: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Accept {
    /// Events are recorded only
    Off,
    /// The Bot sees events and decides, but spawning tasks and replying are only recorded
    #[default]
    Observe,
    Auto,
}

/// What happens to plugin events nobody addressed to the Bot, from senders not on the allow list (chatter in a
/// group the plugin listens to in full). Follow-ups in a task's conversation go to the task either way.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Ambient {
    /// Recorded without waking the Bot; it can still read them with read_thread
    #[default]
    Record,
    /// Delivered to the Bot like any other event
    Deliver,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Allow {
    pub instance: String,
    #[serde(default)]
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Budget {
    /// 0: no limit
    #[serde(default)]
    pub tasks_per_day: u32,
    /// Tokens (Bot plus its tasks) per day before the owner is reminded once; 0: no reminder
    #[serde(default)]
    pub tokens_per_day: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Routine {
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// Five-field cron expression, local time
    pub schedule: String,
    /// What to do on each run
    pub prompt: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    /// Replies into conversations are drafted for approval
    #[serde(default)]
    pub reply: bool,
    /// Plugin tool calls that change something are drafted for approval
    #[serde(default)]
    pub write: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActiveHours {
    /// ISO weekdays, 1 = Monday; empty: every day
    #[serde(default)]
    pub days: Vec<u8>,
    /// "HH:MM"; an end before the start spans midnight
    pub start: String,
    pub end: String,
}

impl ActiveHours {
    fn minutes(s: &str) -> Option<u32> {
        let (h, m) = s.trim().split_once(':')?;
        let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
        (h < 24 && m < 60).then_some(h * 60 + m)
    }

    pub fn check(&self) -> Result<()> {
        let (Some(s), Some(e)) = (Self::minutes(&self.start), Self::minutes(&self.end)) else {
            bail!("活跃时段的时间格式应为 HH:MM");
        };
        if s == e {
            bail!("活跃时段的开始和结束不能相同");
        }
        if self.days.iter().any(|d| !(1..=7).contains(d)) {
            bail!("活跃时段的星期应为 1-7（1 是周一）");
        }
        Ok(())
    }

    /// Whether `at` falls inside the hours (a window spanning midnight belongs to the day it starts)
    pub fn contains(&self, at: chrono::NaiveDateTime) -> bool {
        use chrono::{Datelike, Timelike};
        let (Some(s), Some(e)) = (Self::minutes(&self.start), Self::minutes(&self.end)) else {
            return true;
        };
        let m = at.hour() * 60 + at.minute();
        let day = |d: chrono::NaiveDateTime| self.days.is_empty() || self.days.contains(&(d.weekday().number_from_monday() as u8));
        if s < e {
            day(at) && m >= s && m < e
        } else if m >= s {
            day(at)
        } else {
            m < e && day(at - chrono::Duration::days(1))
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PromptSections {
    /// What this Bot is responsible for and where it stops
    #[serde(default)]
    pub role: String,
    /// When and how to speak
    #[serde(default)]
    pub communication: String,
    /// What a finished result must include
    #[serde(default)]
    pub delivery: String,
}

impl BotConfig {
    /// Fill in what a new Bot gets by default
    pub fn normalize(&mut self) {
        self.name = self.name.trim().to_string();
        if self.notify.is_empty() {
            self.notify = default_notify();
        }
        self.compact_at = self.compact_at.clamp(30, 95);
        self.task.max_concurrent = self.task.max_concurrent.clamp(1, 20);
        for w in &mut self.workspaces {
            w.cwd = w.cwd.trim().to_string();
            w.name = w.name.trim().to_string();
            w.about = w.about.trim().to_string();
        }
        self.bot.cwd = self.bot.cwd.trim().to_string();
        // Keep given ids (the first one wins), then number the rest
        let mut used = std::collections::HashSet::new();
        for r in &mut self.routines {
            r.name = r.name.trim().to_string();
            r.schedule = r.schedule.split_whitespace().collect::<Vec<_>>().join(" ");
            r.id = r.id.trim().to_string();
            if !r.id.is_empty() && !used.insert(r.id.clone()) {
                r.id.clear();
            }
        }
        for r in &mut self.routines {
            if r.id.is_empty() {
                r.id = (1..).map(|n| format!("r{n}")).find(|i| !used.contains(i)).unwrap();
                used.insert(r.id.clone());
            }
        }
        let mut seen = vec![self.task.tool.clone()];
        self.task.tools.retain(|t| !seen.contains(t) && {
            seen.push(t.clone());
            true
        });
    }

    /// Tools a task may use: the default first
    pub fn task_tools(&self) -> Vec<&str> {
        std::iter::once(self.task.tool.as_str()).chain(self.task.tools.iter().map(String::as_str)).collect()
    }

    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() || self.name.chars().count() > 40 {
            bail!("Bot 名称不能为空，最多 40 个字");
        }
        if !AI_TOOLS.contains(&self.bot.tool.as_str()) {
            bail!("Bot 只能使用支持 MCP 的 AI 工具: {}", self.bot.tool);
        }
        if !AI_TOOLS.contains(&self.task.tool.as_str()) {
            bail!("任务只能使用支持 MCP 的 AI 工具: {}", self.task.tool);
        }
        if let Some(t) = self.task.tools.iter().find(|t| !AI_TOOLS.contains(&t.as_str())) {
            bail!("任务只能使用支持 MCP 的 AI 工具: {t}");
        }
        if self.bot.host.is_empty() {
            bail!("请选择 Bot 所在的主机");
        }
        for w in &self.workspaces {
            if w.host.is_empty() || w.cwd.is_empty() {
                bail!("工作区需要主机和目录");
            }
        }
        for r in &self.routines {
            if r.name.is_empty() || r.prompt.trim().is_empty() {
                bail!("例行任务需要名称和要做的事");
            }
            if let Err(e) = crate::plugins::cron::Cron::parse(&r.schedule) {
                bail!("例行任务「{}」的时间表无效: {e}", r.name);
            }
        }
        if let Some(h) = &self.active_hours {
            h.check()?;
        }
        Ok(())
    }

    /// Find a workspace by index, name, exact path, or folder basename
    pub fn find_workspace(&self, v: &serde_json::Value) -> Option<&Workspace> {
        if let Some(i) = v.as_u64() {
            return self.workspaces.get(i as usize);
        }
        let s = v.as_str()?.trim();
        if s.is_empty() {
            return None;
        }
        self.workspaces
            .iter()
            .find(|w| !w.name.is_empty() && w.name.eq_ignore_ascii_case(s))
            .or_else(|| self.workspaces.iter().find(|w| w.cwd == s))
            .or_else(|| {
                self.workspaces.iter().find(|w| {
                    w.cwd.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next() == Some(s)
                })
            })
            .or_else(|| s.parse::<usize>().ok().and_then(|i| self.workspaces.get(i)))
    }

    /// Whether `sender` on `instance` is on the allow list
    pub fn allowed(&self, instance: &str, sender: &str) -> bool {
        self.allow.iter().any(|a| a.instance == instance && a.ids.iter().any(|i| i == sender))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_minimal_bot_gets_safe_defaults() {
        let mut c: BotConfig = serde_json::from_value(json!({
            "name": " Issue helper ",
            "bot": {"host": "local", "tool": "claude"}
        }))
        .unwrap();
        c.normalize();
        c.validate().unwrap();
        assert_eq!(c.name, "Issue helper");
        assert!(!c.enabled);
        assert_eq!(c.accept, Accept::Observe);
        assert_eq!(c.ambient, Ambient::Record);
        assert_eq!(c.task.permission_mode, "bypass");
        assert!(!c.task.computer_use);
        assert_eq!(c.notify, ["app"]);
        assert_eq!(c.compact_at, 70);
    }

    #[test]
    fn shell_and_unknown_tools_are_rejected() {
        let mut c: BotConfig = serde_json::from_value(json!({"name": "x", "bot": {"host": "local", "tool": "shell"}})).unwrap();
        assert!(c.validate().is_err());
        c.bot.tool = "codex".into();
        c.task.tool = "files".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn extra_task_tools_drop_duplicates_and_reject_unknown_ones() {
        let mut c: BotConfig = serde_json::from_value(json!({
            "name": "x",
            "bot": {"host": "local", "tool": "claude"},
            "task": {"tool": "codex", "tools": ["claude", "codex", "claude", "kiro"]}
        }))
        .unwrap();
        c.normalize();
        c.validate().unwrap();
        assert_eq!(c.task_tools(), ["codex", "claude", "kiro"]);
        c.task.tools.push("shell".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn routines_get_unique_ids_and_bad_schedules_are_rejected() {
        let mut c: BotConfig = serde_json::from_value(json!({
            "name": "x",
            "bot": {"host": "local", "tool": "claude"},
            "routines": [
                {"name": "a", "schedule": "0  9 * * 1-5", "prompt": "p"},
                {"id": "r1", "name": "b", "schedule": "*/30 * * * *", "prompt": "p"},
                {"id": "keep", "name": "c", "schedule": "0 18 * * *", "prompt": "p", "enabled": false}
            ]
        }))
        .unwrap();
        c.normalize();
        c.validate().unwrap();
        let ids: Vec<_> = c.routines.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        assert!(ids[0] != ids[1] && ids[2] == "keep");
        assert_eq!(c.routines[0].schedule, "0 9 * * 1-5");
        assert!(c.routines[0].enabled && !c.routines[2].enabled);
        c.routines[1].schedule = "every morning".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn active_hours_can_span_midnight() {
        use chrono::NaiveDate;
        let at = |d: u32, h: u32, m: u32| NaiveDate::from_ymd_opt(2026, 11, d).unwrap().and_hms_opt(h, m, 0).unwrap();
        // 2026-11-09 is a Monday
        let work = ActiveHours { days: vec![1, 2, 3, 4, 5], start: "09:00".into(), end: "18:00".into() };
        work.check().unwrap();
        assert!(work.contains(at(9, 9, 0)));
        assert!(!work.contains(at(9, 18, 0)));
        assert!(!work.contains(at(15, 10, 0)));
        let night = ActiveHours { days: vec![5], start: "22:00".into(), end: "02:00".into() };
        assert!(night.contains(at(13, 23, 0)));
        assert!(night.contains(at(14, 1, 30)));
        assert!(!night.contains(at(13, 1, 30)));
        assert!(ActiveHours { days: vec![], start: "9".into(), end: "18:00".into() }.check().is_err());
    }

    #[test]
    fn find_workspace_matches_by_index_name_path_or_basename() {
        let mut c: BotConfig = serde_json::from_value(json!({
            "name": "x",
            "bot": {"host": "local", "tool": "claude"},
            "workspaces": [
                {"host": "local", "cwd": "/git/yi-console", "name": " console ", "about": " desktop app "},
                {"host": "mac", "cwd": "/Users/me/docs"}
            ]
        }))
        .unwrap();
        c.normalize();
        assert_eq!(c.workspaces[0].name, "console");
        assert_eq!(c.workspaces[0].about, "desktop app");
        assert_eq!(c.find_workspace(&json!(1)).unwrap().cwd, "/Users/me/docs");
        assert_eq!(c.find_workspace(&json!("Console")).unwrap().cwd, "/git/yi-console");
        assert_eq!(c.find_workspace(&json!("/Users/me/docs")).unwrap().host, "mac");
        assert_eq!(c.find_workspace(&json!("yi-console")).unwrap().name, "console");
        assert_eq!(c.find_workspace(&json!("0")).unwrap().name, "console");
        assert!(c.find_workspace(&json!("missing")).is_none());
        assert!(c.find_workspace(&json!("")).is_none());
    }
}
