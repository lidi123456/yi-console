//! Local database of the desktop app's Bots (`<app data>/bots.db`).

use crate::config::{BotConfig, PromptSections};
use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::sync::Mutex;

pub struct Store {
    db: Mutex<Connection>,
}

/// Where a Bot's own session lives
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionRef {
    pub host: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotRow {
    pub config: BotConfig,
    pub notes: String,
    pub session: Option<SessionRef>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRevision {
    pub revision: u32,
    pub base_revision: u32,
    pub sections: PromptSections,
    /// form / ai / bot
    pub origin: String,
    pub at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub id: String,
    pub plugin_id: String,
    pub name: String,
    /// Secrets in plain text, like the hosts' MCP env; redacted before it leaves the engine
    pub config: Value,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRow {
    pub id: String,
    pub instance_id: String,
    pub bot_id: String,
    pub key: String,
    pub thread: String,
    pub sender: String,
    #[serde(default)]
    pub sender_name: String,
    pub sender_bot: bool,
    pub addressed: bool,
    pub text: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub meta: Value,
    pub at: i64,
    /// filtered / observed / queued / delivered / attached / ignored / spawned
    pub disposition: String,
    #[serde(default)]
    pub task_id: Option<String>,
    /// Why it was filtered / ignored, or what observe mode would have done
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRow {
    pub id: String,
    pub bot_id: String,
    pub host: String,
    #[serde(default)]
    pub session_id: Option<String>,
    /// queued / working / needsYou / ready / failed / done / stopped
    pub state: String,
    pub title: String,
    #[serde(default)]
    pub event_ids: Vec<String>,
    #[serde(default)]
    pub threads: Vec<String>,
    /// Directory the task works in (the worktree if one was made)
    #[serde(default)]
    pub cwd: String,
    /// The workspace directory it came from
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub worktree: Option<Value>,
    /// Tool, model, effort, note and brief the task was spawned with
    #[serde(default)]
    pub spec: Value,
    #[serde(default)]
    pub last_report: Option<Value>,
    /// Stopped by a person: nothing restarts it automatically
    #[serde(default)]
    pub hold: bool,
    /// A person spoke in the task: Bot instructions wait until it is handed back
    #[serde(default)]
    pub manual: bool,
    /// History position up to which offline `yi_task` calls were caught up
    #[serde(default)]
    pub seen_seq: i64,
    pub created_at: i64,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub ended_at: Option<i64>,
    /// When the session (and a clean worktree) of the ended task were removed
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleaned_at: Option<i64>,
}

impl TaskRow {
    pub fn active(&self) -> bool {
        matches!(self.state.as_str(), "queued" | "working" | "needsYou")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attention {
    pub id: String,
    pub bot_id: String,
    #[serde(default)]
    pub task_id: Option<String>,
    pub text: String,
    /// low / normal / high
    pub urgency: String,
    pub at: i64,
}

/// A message on its way out through a plugin (a reply into a conversation, or a notification). The key is the
/// idempotency key the plugin gets, so retries never send twice
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxRow {
    pub key: String,
    pub instance_id: String,
    /// reply / notify
    pub kind: String,
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub thread: String,
    #[serde(default)]
    pub reply_to: Option<String>,
    pub text: String,
    /// pending / sent / gone / failed
    pub state: String,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub error: Option<String>,
    pub next_at: i64,
    pub created_at: i64,
}

/// One run of a routine. Only the latest `RUNS_KEPT` per routine are kept
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutineRun {
    pub id: String,
    pub bot_id: String,
    pub routine_id: String,
    pub name: String,
    /// queued / running / done / nothing / failed / skipped
    pub state: String,
    /// Started by "Run now" rather than the schedule
    #[serde(default)]
    pub manual: bool,
    #[serde(default)]
    pub summary: String,
    pub started_at: i64,
    #[serde(default)]
    pub ended_at: Option<i64>,
}

pub const RUNS_KEPT: usize = 20;

/// An outgoing action the Bot drafted that waits for the owner
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRow {
    pub id: String,
    pub bot_id: String,
    /// reply / tool
    pub kind: String,
    /// pending / approved / rejected / expired
    pub state: String,
    pub instance_id: String,
    /// reply: the event answered, its conversation and message
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub thread: String,
    #[serde(default)]
    pub reply_to: Option<String>,
    /// reply: the text to send; tool: what the call does, in the Bot's words
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub input: Value,
    /// For the card: the plugin instance's name, and the message a reply answers (who, what)
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub quote_from: String,
    #[serde(default)]
    pub quote: String,
    /// What carrying it out gave (sent, the tool's answer, an error)
    #[serde(default)]
    pub result: Option<String>,
    pub created_at: i64,
    #[serde(default)]
    pub decided_at: Option<i64>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS bots (
                 id TEXT PRIMARY KEY, config TEXT NOT NULL, notes TEXT NOT NULL DEFAULT '',
                 session TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS bot_prompts (
                 bot_id TEXT NOT NULL, revision INTEGER NOT NULL, base_revision INTEGER NOT NULL,
                 sections TEXT NOT NULL, origin TEXT NOT NULL, at INTEGER NOT NULL,
                 PRIMARY KEY (bot_id, revision));
             CREATE TABLE IF NOT EXISTS plugin_instances (
                 id TEXT PRIMARY KEY, plugin_id TEXT NOT NULL, name TEXT NOT NULL, config TEXT NOT NULL,
                 enabled INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS plugin_kv (
                 instance_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
                 PRIMARY KEY (instance_id, key));
             CREATE TABLE IF NOT EXISTS trigger_events (
                 id TEXT PRIMARY KEY, instance_id TEXT NOT NULL, bot_id TEXT NOT NULL, key TEXT NOT NULL,
                 at INTEGER NOT NULL, disposition TEXT NOT NULL, task_id TEXT, data TEXT NOT NULL,
                 UNIQUE (instance_id, bot_id, key));
             CREATE INDEX IF NOT EXISTS trigger_events_bot ON trigger_events (bot_id, at);
             CREATE TABLE IF NOT EXISTS tasks (
                 id TEXT PRIMARY KEY, bot_id TEXT NOT NULL, state TEXT NOT NULL, created_at INTEGER NOT NULL,
                 data TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS tasks_bot ON tasks (bot_id, created_at);
             CREATE TABLE IF NOT EXISTS attention (
                 id TEXT PRIMARY KEY, bot_id TEXT NOT NULL, task_id TEXT, text TEXT NOT NULL,
                 urgency TEXT NOT NULL, at INTEGER NOT NULL, resolved_at INTEGER);
             CREATE TABLE IF NOT EXISTS outbox (
                 key TEXT PRIMARY KEY, instance_id TEXT NOT NULL, state TEXT NOT NULL, next_at INTEGER NOT NULL,
                 created_at INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS outbox_due ON outbox (state, next_at);
             CREATE TABLE IF NOT EXISTS routine_runs (
                 id TEXT PRIMARY KEY, bot_id TEXT NOT NULL, routine_id TEXT NOT NULL, state TEXT NOT NULL,
                 started_at INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS routine_runs_bot ON routine_runs (bot_id, routine_id, started_at);
             CREATE TABLE IF NOT EXISTS approvals (
                 id TEXT PRIMARY KEY, bot_id TEXT NOT NULL, state TEXT NOT NULL, created_at INTEGER NOT NULL,
                 data TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS approvals_state ON approvals (state, created_at);",
        )?;
        Ok(Store { db: Mutex::new(db) })
    }

    // ---------- bots ----------

    pub fn bots(&self) -> Result<Vec<BotRow>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare("SELECT config, notes, session, created_at, updated_at FROM bots ORDER BY created_at")?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get(3)?, r.get(4)?))
        })?;
        let mut out = vec![];
        for row in rows {
            let (config, notes, session, created_at, updated_at) = row?;
            let Ok(config) = serde_json::from_str(&config) else { continue };
            out.push(BotRow {
                config,
                notes,
                session: session.and_then(|s| serde_json::from_str(&s).ok()),
                created_at,
                updated_at,
            });
        }
        Ok(out)
    }

    pub fn bot(&self, id: &str) -> Result<BotRow> {
        self.bots()?.into_iter().find(|b| b.config.id == id).ok_or_else(|| anyhow!("Bot 不存在: {id}"))
    }

    pub fn save_bot(&self, c: &BotConfig, now: i64) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO bots (id, config, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(id) DO UPDATE SET config = excluded.config, updated_at = excluded.updated_at",
            params![c.id, serde_json::to_string(c)?, now],
        )?;
        Ok(())
    }

    pub fn set_bot_session(&self, id: &str, s: Option<&SessionRef>) -> Result<()> {
        let v = s.map(serde_json::to_string).transpose()?;
        self.db.lock().unwrap().execute("UPDATE bots SET session = ?2 WHERE id = ?1", params![id, v])?;
        Ok(())
    }

    pub fn set_notes(&self, id: &str, notes: &str) -> Result<()> {
        self.db.lock().unwrap().execute("UPDATE bots SET notes = ?2 WHERE id = ?1", params![id, notes])?;
        Ok(())
    }

    pub fn delete_bot(&self, id: &str) -> Result<()> {
        let db = self.db.lock().unwrap();
        for sql in [
            "DELETE FROM bots WHERE id = ?1",
            "DELETE FROM bot_prompts WHERE bot_id = ?1",
            "DELETE FROM trigger_events WHERE bot_id = ?1",
            "DELETE FROM tasks WHERE bot_id = ?1",
            "DELETE FROM attention WHERE bot_id = ?1",
            "DELETE FROM routine_runs WHERE bot_id = ?1",
            "DELETE FROM approvals WHERE bot_id = ?1",
            // The engine's own counters (tokens) live next to plugin kv
            "DELETE FROM plugin_kv WHERE instance_id = 'bot:' || ?1",
        ] {
            db.execute(sql, params![id])?;
        }
        Ok(())
    }

    // ---------- prompts ----------

    /// Store a new prompt revision; returns its number
    pub fn add_prompt(&self, bot_id: &str, base: u32, sections: &PromptSections, origin: &str, now: i64) -> Result<u32> {
        let db = self.db.lock().unwrap();
        let last: u32 = db
            .query_row("SELECT COALESCE(MAX(revision), 0) FROM bot_prompts WHERE bot_id = ?1", params![bot_id], |r| r.get(0))?;
        db.execute(
            "INSERT INTO bot_prompts (bot_id, revision, base_revision, sections, origin, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![bot_id, last + 1, base, serde_json::to_string(sections)?, origin, now],
        )?;
        Ok(last + 1)
    }

    pub fn prompts(&self, bot_id: &str) -> Result<Vec<PromptRevision>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT revision, base_revision, sections, origin, at FROM bot_prompts WHERE bot_id = ?1 ORDER BY revision DESC",
        )?;
        let rows = st.query_map(params![bot_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get::<_, String>(2)?, r.get(3)?, r.get(4)?))
        })?;
        let mut out = vec![];
        for row in rows {
            let (revision, base_revision, sections, origin, at) = row?;
            out.push(PromptRevision {
                revision,
                base_revision,
                sections: serde_json::from_str(&sections).unwrap_or_default(),
                origin,
                at,
            });
        }
        Ok(out)
    }

    // ---------- plugin instances ----------

    pub fn instances(&self) -> Result<Vec<Instance>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare("SELECT id, plugin_id, name, config, enabled FROM plugin_instances ORDER BY created_at")?;
        let rows = st.query_map([], |r| {
            Ok(Instance {
                id: r.get(0)?,
                plugin_id: r.get(1)?,
                name: r.get(2)?,
                config: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(Value::Null),
                enabled: r.get::<_, i64>(4)? != 0,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn save_instance(&self, i: &Instance, now: i64) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO plugin_instances (id, plugin_id, name, config, enabled, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, config = excluded.config, enabled = excluded.enabled",
            params![i.id, i.plugin_id, i.name, serde_json::to_string(&i.config)?, i.enabled as i64, now],
        )?;
        Ok(())
    }

    pub fn delete_instance(&self, id: &str) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute("DELETE FROM plugin_instances WHERE id = ?1", params![id])?;
        db.execute("DELETE FROM plugin_kv WHERE instance_id = ?1", params![id])?;
        Ok(())
    }

    pub fn kv_get(&self, instance: &str, key: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM plugin_kv WHERE instance_id = ?1 AND key = ?2",
                params![instance, key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn kv_set(&self, instance: &str, key: &str, value: Option<&str>) -> Result<()> {
        let db = self.db.lock().unwrap();
        match value {
            Some(v) => db.execute(
                "INSERT INTO plugin_kv (instance_id, key, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT(instance_id, key) DO UPDATE SET value = excluded.value",
                params![instance, key, v],
            )?,
            None => db.execute("DELETE FROM plugin_kv WHERE instance_id = ?1 AND key = ?2", params![instance, key])?,
        };
        Ok(())
    }

    // ---------- events ----------

    /// Insert an event for a Bot; false if that Bot already has an event with the same source key
    pub fn add_event(&self, e: &EventRow) -> Result<bool> {
        let n = self.db.lock().unwrap().execute(
            "INSERT OR IGNORE INTO trigger_events (id, instance_id, bot_id, key, at, disposition, task_id, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![e.id, e.instance_id, e.bot_id, e.key, e.at, e.disposition, e.task_id, serde_json::to_string(e)?],
        )?;
        Ok(n > 0)
    }

    pub fn update_event(&self, e: &EventRow) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE trigger_events SET disposition = ?2, task_id = ?3, data = ?4 WHERE id = ?1",
            params![e.id, e.disposition, e.task_id, serde_json::to_string(e)?],
        )?;
        Ok(())
    }

    pub fn event(&self, id: &str) -> Result<EventRow> {
        let data: String = self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT data FROM trigger_events WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| anyhow!("事件不存在: {id}"))?;
        Ok(serde_json::from_str(&data)?)
    }

    /// Newest first; `before` pages back by time, `disposition` keeps only that kind
    pub fn events(&self, bot_id: &str, before: Option<i64>, limit: usize, disposition: Option<&str>) -> Result<Vec<EventRow>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT data FROM trigger_events WHERE bot_id = ?1 AND at < ?2 AND (?4 IS NULL OR disposition = ?4)
             ORDER BY at DESC LIMIT ?3",
        )?;
        let rows = st.query_map(params![bot_id, before.unwrap_or(i64::MAX), limit as i64, disposition], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            if let Ok(e) = serde_json::from_str(&r?) {
                out.push(e);
            }
        }
        Ok(out)
    }

    // ---------- tasks ----------

    pub fn save_task(&self, t: &TaskRow) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO tasks (id, bot_id, state, created_at, data) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET state = excluded.state, data = excluded.data",
            params![t.id, t.bot_id, t.state, t.created_at, serde_json::to_string(t)?],
        )?;
        Ok(())
    }

    pub fn task(&self, id: &str) -> Result<TaskRow> {
        let data: String = self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT data FROM tasks WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| anyhow!("任务不存在: {id}"))?;
        Ok(serde_json::from_str(&data)?)
    }

    /// A Bot's tasks (all Bots when None), newest first
    pub fn tasks(&self, bot_id: Option<&str>, limit: usize) -> Result<Vec<TaskRow>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT data FROM tasks WHERE (?1 IS NULL OR bot_id = ?1) ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = st.query_map(params![bot_id, limit as i64], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            if let Ok(t) = serde_json::from_str(&r?) {
                out.push(t);
            }
        }
        Ok(out)
    }

    pub fn active_tasks(&self) -> Result<Vec<TaskRow>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT data FROM tasks WHERE state IN ('queued', 'working', 'needsYou') ORDER BY created_at",
        )?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            if let Ok(t) = serde_json::from_str(&r?) {
                out.push(t);
            }
        }
        Ok(out)
    }

    /// Ended tasks not cleaned up yet
    pub fn untidy_tasks(&self) -> Result<Vec<TaskRow>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT data FROM tasks WHERE state IN ('done', 'failed', 'stopped') AND json_extract(data, '$.cleanedAt') IS NULL",
        )?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            if let Ok(t) = serde_json::from_str(&r?) {
                out.push(t);
            }
        }
        Ok(out)
    }

    /// (time, disposition) of the events and (time, state) of the tasks since `since`, for statistics
    pub fn activity_since(&self, bot_id: &str, since: i64) -> Result<(Vec<(i64, String)>, Vec<(i64, String)>)> {
        let db = self.db.lock().unwrap();
        let pairs = |sql: &str| -> Result<Vec<(i64, String)>> {
            let mut st = db.prepare(sql)?;
            let rows = st.query_map(params![bot_id, since], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        };
        let events = pairs("SELECT at, disposition FROM trigger_events WHERE bot_id = ?1 AND at >= ?2")?;
        let tasks = pairs("SELECT created_at, state FROM tasks WHERE bot_id = ?1 AND created_at >= ?2")?;
        Ok((events, tasks))
    }

    pub fn tasks_since(&self, bot_id: &str, since: i64) -> Result<u32> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM tasks WHERE bot_id = ?1 AND created_at >= ?2",
            params![bot_id, since],
            |r| r.get(0),
        )?)
    }

    // ---------- outbox ----------

    /// false: the key is already there (the earlier attempt stands)
    pub fn outbox_add(&self, o: &OutboxRow) -> Result<bool> {
        let n = self.db.lock().unwrap().execute(
            "INSERT OR IGNORE INTO outbox (key, instance_id, state, next_at, created_at, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![o.key, o.instance_id, o.state, o.next_at, o.created_at, serde_json::to_string(o)?],
        )?;
        Ok(n > 0)
    }

    pub fn outbox_save(&self, o: &OutboxRow) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE outbox SET state = ?2, next_at = ?3, data = ?4 WHERE key = ?1",
            params![o.key, o.state, o.next_at, serde_json::to_string(o)?],
        )?;
        Ok(())
    }

    pub fn outbox_get(&self, key: &str) -> Result<OutboxRow> {
        let data: String = self.db.lock().unwrap().query_row("SELECT data FROM outbox WHERE key = ?1", params![key], |r| r.get(0))?;
        Ok(serde_json::from_str(&data)?)
    }

    /// Pending messages due for an attempt
    pub fn outbox_due(&self, now: i64) -> Result<Vec<OutboxRow>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare("SELECT data FROM outbox WHERE state = 'pending' AND next_at <= ?1 ORDER BY created_at LIMIT 50")?;
        let rows = st.query_map(params![now], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).filter_map(|d| serde_json::from_str(&d).ok()).collect())
    }

    /// Newest first, for the plugins page
    pub fn outbox(&self, instance_id: Option<&str>, limit: usize) -> Result<Vec<OutboxRow>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare("SELECT data FROM outbox WHERE ?1 IS NULL OR instance_id = ?1 ORDER BY created_at DESC LIMIT ?2")?;
        let rows = st.query_map(params![instance_id, limit as i64], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).filter_map(|d| serde_json::from_str(&d).ok()).collect())
    }

    /// Forget everything about an instance (uninstalling without keeping data)
    pub fn purge_instance(&self, id: &str) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute("DELETE FROM plugin_kv WHERE instance_id = ?1", params![id])?;
        db.execute("DELETE FROM outbox WHERE instance_id = ?1", params![id])?;
        db.execute("DELETE FROM plugin_instances WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ---------- attention ----------

    pub fn add_attention(&self, a: &Attention) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO attention (id, bot_id, task_id, text, urgency, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![a.id, a.bot_id, a.task_id, a.text, a.urgency, a.at],
        )?;
        Ok(())
    }

    pub fn attention(&self, bot_id: Option<&str>) -> Result<Vec<Attention>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT id, bot_id, task_id, text, urgency, at FROM attention
             WHERE resolved_at IS NULL AND (?1 IS NULL OR bot_id = ?1) ORDER BY at DESC",
        )?;
        let rows = st.query_map(params![bot_id], |r| {
            Ok(Attention {
                id: r.get(0)?,
                bot_id: r.get(1)?,
                task_id: r.get(2)?,
                text: r.get(3)?,
                urgency: r.get(4)?,
                at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Resolve one item, or everything about a task
    pub fn resolve_attention(&self, id: Option<&str>, task_id: Option<&str>, now: i64) -> Result<()> {
        let db = self.db.lock().unwrap();
        if let Some(id) = id {
            db.execute("UPDATE attention SET resolved_at = ?2 WHERE id = ?1", params![id, now])?;
        }
        if let Some(t) = task_id {
            db.execute("UPDATE attention SET resolved_at = ?2 WHERE task_id = ?1 AND resolved_at IS NULL", params![t, now])?;
        }
        Ok(())
    }

    /// Resolve everything open for a Bot
    pub fn resolve_bot_attention(&self, bot_id: &str, now: i64) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute("UPDATE attention SET resolved_at = ?2 WHERE bot_id = ?1 AND resolved_at IS NULL", params![bot_id, now])?;
        Ok(())
    }

    // ---------- routine runs ----------

    /// Insert or update a run; a new one drops the routine's runs past `RUNS_KEPT`
    pub fn save_run(&self, r: &RoutineRun) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR REPLACE INTO routine_runs (id, bot_id, routine_id, state, started_at, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![r.id, r.bot_id, r.routine_id, r.state, r.started_at, serde_json::to_string(r)?],
        )?;
        db.execute(
            "DELETE FROM routine_runs WHERE bot_id = ?1 AND routine_id = ?2 AND id NOT IN
             (SELECT id FROM routine_runs WHERE bot_id = ?1 AND routine_id = ?2 ORDER BY started_at DESC LIMIT ?3)",
            params![r.bot_id, r.routine_id, RUNS_KEPT as i64],
        )?;
        Ok(())
    }

    pub fn run(&self, id: &str) -> Result<RoutineRun> {
        let db = self.db.lock().unwrap();
        let data: String = db
            .query_row("SELECT data FROM routine_runs WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| anyhow!("找不到这次运行"))?;
        Ok(serde_json::from_str(&data)?)
    }

    /// Runs of a Bot, newest first; one routine's when `routine_id` is given
    pub fn runs(&self, bot_id: &str, routine_id: Option<&str>, limit: usize) -> Result<Vec<RoutineRun>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT data FROM routine_runs WHERE bot_id = ?1 AND (?2 IS NULL OR routine_id = ?2) ORDER BY started_at DESC LIMIT ?3",
        )?;
        let rows = st.query_map(params![bot_id, routine_id, limit as i64], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok().and_then(|d| serde_json::from_str(&d).ok())).collect())
    }

    /// Runs that haven't ended (queued or running)
    pub fn open_runs(&self) -> Result<Vec<RoutineRun>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare("SELECT data FROM routine_runs WHERE state IN ('queued', 'running')")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok().and_then(|d| serde_json::from_str(&d).ok())).collect())
    }

    // ---------- approvals ----------

    pub fn save_approval(&self, a: &ApprovalRow) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT OR REPLACE INTO approvals (id, bot_id, state, created_at, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![a.id, a.bot_id, a.state, a.created_at, serde_json::to_string(a)?],
        )?;
        Ok(())
    }

    pub fn approval(&self, id: &str) -> Result<ApprovalRow> {
        let db = self.db.lock().unwrap();
        let data: String = db
            .query_row("SELECT data FROM approvals WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| anyhow!("找不到这条待批准的操作"))?;
        Ok(serde_json::from_str(&data)?)
    }

    /// Newest first; filtered by Bot and state when given
    pub fn approvals(&self, bot_id: Option<&str>, state: Option<&str>, limit: usize) -> Result<Vec<ApprovalRow>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT data FROM approvals WHERE (?1 IS NULL OR bot_id = ?1) AND (?2 IS NULL OR state = ?2)
             ORDER BY created_at DESC LIMIT ?3",
        )?;
        let rows = st.query_map(params![bot_id, state, limit as i64], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok().and_then(|d| serde_json::from_str(&d).ok())).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bot(id: &str) -> BotConfig {
        let mut c: BotConfig = serde_json::from_value(json!({"name": "b", "bot": {"host": "local", "tool": "claude"}})).unwrap();
        c.id = id.into();
        c
    }

    #[test]
    fn bots_tasks_events_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("bots.db")).unwrap();
        s.save_bot(&bot("b1"), 1).unwrap();
        s.set_bot_session("b1", Some(&SessionRef { host: "local".into(), session_id: "s".into() })).unwrap();
        assert_eq!(s.bot("b1").unwrap().session.unwrap().session_id, "s");
        assert_eq!(s.add_prompt("b1", 0, &Default::default(), "form", 1).unwrap(), 1);
        assert_eq!(s.add_prompt("b1", 1, &Default::default(), "ai", 2).unwrap(), 2);
        assert_eq!(s.prompts("b1").unwrap()[0].origin, "ai");

        let e = EventRow { id: "e1".into(), instance_id: "i".into(), bot_id: "b1".into(), key: "k".into(), at: 5, disposition: "queued".into(), ..Default::default() };
        assert!(s.add_event(&e).unwrap());
        // Same source key for the same Bot is a duplicate
        assert!(!s.add_event(&EventRow { id: "e2".into(), ..e.clone() }).unwrap());
        assert_eq!(s.events("b1", None, 10, None).unwrap().len(), 1);
        assert_eq!(s.events("b1", None, 10, Some("queued")).unwrap().len(), 1);
        assert!(s.events("b1", None, 10, Some("ignored")).unwrap().is_empty());

        let t = TaskRow { id: "t1".into(), bot_id: "b1".into(), state: "working".into(), created_at: 10, ..Default::default() };
        s.save_task(&t).unwrap();
        assert_eq!(s.active_tasks().unwrap().len(), 1);
        s.save_task(&TaskRow { state: "done".into(), ..t }).unwrap();
        assert!(s.active_tasks().unwrap().is_empty());
        assert_eq!(s.tasks_since("b1", 0).unwrap(), 1);
        let (ev, ts) = s.activity_since("b1", 0).unwrap();
        assert_eq!((ev, ts), (vec![(5, "queued".to_string())], vec![(10, "done".to_string())]));

        s.add_attention(&Attention { id: "a".into(), bot_id: "b1".into(), task_id: Some("t1".into()), text: "?".into(), urgency: "normal".into(), at: 1 }).unwrap();
        assert_eq!(s.attention(None).unwrap().len(), 1);
        s.resolve_attention(None, Some("t1"), 2).unwrap();
        assert!(s.attention(Some("b1")).unwrap().is_empty());

        let o = OutboxRow { key: "k1".into(), instance_id: "i".into(), kind: "reply".into(), text: "hi".into(), state: "pending".into(), next_at: 5, created_at: 1, ..Default::default() };
        assert!(s.outbox_add(&o).unwrap());
        assert!(!s.outbox_add(&o).unwrap());
        assert_eq!(s.outbox_due(4).unwrap().len(), 0);
        assert_eq!(s.outbox_due(5).unwrap().len(), 1);
        s.outbox_save(&OutboxRow { state: "sent".into(), ..o }).unwrap();
        assert!(s.outbox_due(10).unwrap().is_empty());
        assert_eq!(s.outbox_get("k1").unwrap().state, "sent");
        s.purge_instance("i").unwrap();
        assert!(s.outbox(None, 10).unwrap().is_empty());

        s.delete_bot("b1").unwrap();
        assert!(s.bots().unwrap().is_empty());
        assert!(s.tasks(None, 10).unwrap().is_empty());

    }
}
