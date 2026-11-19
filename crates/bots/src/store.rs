//! Local database of the desktop app's Bots (`<app data>/bots.db`).

use crate::config::{BotConfig, PromptSections, Sop};
use anyhow::{anyhow, bail, Result};
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
    #[serde(default)]
    pub sops: Vec<Sop>,
    /// form / ai / bot / create / restore
    pub origin: String,
    pub at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptSnapshot {
    #[serde(flatten)]
    sections: PromptSections,
    #[serde(default)]
    sops: Vec<Sop>,
}

/// Durable memory entry scoped either globally (`scope == ""`) or to a canonical workspace (`"<host>:<cwd>"`)
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRow {
    pub id: String,
    pub bot_id: String,
    #[serde(default)]
    pub scope: String,
    pub content: String,
    /// Derived by the engine only: owner / review / task / bot
    #[serde(default)]
    pub source: String,
    pub updated_at: i64,
}

pub const MEMORY_MAX_TOTAL: usize = 80;
pub const MEMORY_MAX_SCOPE: usize = 40;
pub const MEMORY_MAX_CHARS: usize = 300;

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
        matches!(self.state.as_str(), "queued" | "working" | "needsYou" | "reviewing")
    }

    /// A `kind: plugin` task (`spawn_task`): builds a Yi Console plugin in a scratch directory
    pub fn is_plugin(&self) -> bool {
        self.spec["kind"].as_str() == Some("plugin")
    }

    pub fn mode(&self) -> &str {
        match self.spec["mode"].as_str() {
            Some(m @ ("worktree" | "repo" | "scratch")) => m,
            _ if self.worktree.is_some() => "worktree",
            _ if !self.workspace.is_empty() => "repo",
            _ => "scratch",
        }
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
    /// Original drafted text before the owner edited it (set only when edited on approval)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_text: Option<String>,
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
             CREATE INDEX IF NOT EXISTS approvals_state ON approvals (state, created_at);
             CREATE TABLE IF NOT EXISTS bot_memories (
                 id TEXT PRIMARY KEY, bot_id TEXT NOT NULL, scope TEXT NOT NULL DEFAULT '',
                 content TEXT NOT NULL, source TEXT NOT NULL DEFAULT 'bot', updated_at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS bot_memories_bot ON bot_memories (bot_id, scope);",
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
            "DELETE FROM bot_memories WHERE bot_id = ?1",
            "DELETE FROM trigger_events WHERE bot_id = ?1",
            "DELETE FROM tasks WHERE bot_id = ?1",
            "DELETE FROM attention WHERE bot_id = ?1",
            "DELETE FROM routine_runs WHERE bot_id = ?1",
            "DELETE FROM approvals WHERE bot_id = ?1",
            "DELETE FROM outbox WHERE state = 'pending' AND json_extract(data, '$.botId') = ?1",
            // The engine's own counters (tokens) live next to plugin kv
            "DELETE FROM plugin_kv WHERE instance_id = 'bot:' || ?1",
        ] {
            db.execute(sql, params![id])?;
        }
        Ok(())
    }

    // ---------- prompts ----------

    /// Store a new prompt (and SOP) revision; returns its number
    pub fn add_prompt(&self, bot_id: &str, base: u32, sections: &PromptSections, sops: &[Sop], origin: &str, now: i64) -> Result<u32> {
        let db = self.db.lock().unwrap();
        let last: u32 = db
            .query_row("SELECT COALESCE(MAX(revision), 0) FROM bot_prompts WHERE bot_id = ?1", params![bot_id], |r| r.get(0))?;
        let snap = PromptSnapshot { sections: sections.clone(), sops: sops.to_vec() };
        db.execute(
            "INSERT INTO bot_prompts (bot_id, revision, base_revision, sections, origin, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![bot_id, last + 1, base, serde_json::to_string(&snap)?, origin, now],
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
            let (revision, base_revision, raw, origin, at) = row?;
            let snap: PromptSnapshot = serde_json::from_str(&raw).unwrap_or_default();
            out.push(PromptRevision {
                revision,
                base_revision,
                sections: snap.sections,
                sops: snap.sops,
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
        put_event(&self.db.lock().unwrap(), e)
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
        put_task(&self.db.lock().unwrap(), t)
    }

    /// A new task, the events it takes and its acknowledgement in one transaction: a crash can't leave a task whose
    /// events still look unhandled, or one the requester is never told about
    pub fn spawn(&self, t: &TaskRow, events: &[EventRow], ack: Option<&OutboxRow>) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        put_task(&tx, t)?;
        for e in events {
            put_event(&tx, e)?;
        }
        if let Some(o) = ack {
            put_outbox(&tx, o)?;
        }
        tx.commit()?;
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

    /// Every task of one Bot without a row cap (used when deleting a Bot)
    pub fn bot_tasks(&self, bot_id: &str) -> Result<Vec<TaskRow>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare("SELECT data FROM tasks WHERE bot_id = ?1 ORDER BY created_at DESC")?;
        let rows = st.query_map(params![bot_id], |r| r.get::<_, String>(0))?;
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
            "SELECT data FROM tasks WHERE state IN ('queued', 'working', 'needsYou', 'reviewing') ORDER BY created_at",
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
        put_outbox(&self.db.lock().unwrap(), o)
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

    /// Newest first, for the plugins page and Bot details outbox panel
    pub fn outbox(&self, instance_id: Option<&str>, bot_id: Option<&str>, limit: usize) -> Result<Vec<OutboxRow>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT data FROM outbox
             WHERE (?1 IS NULL OR instance_id = ?1)
               AND (?2 IS NULL OR json_extract(data, '$.botId') = ?2)
             ORDER BY created_at DESC LIMIT ?3",
        )?;
        let rows = st.query_map(params![instance_id, bot_id, limit as i64], |r| r.get::<_, String>(0))?;
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

    // ---------- memories ----------

    /// All memories of a Bot (optionally filtered to one exact scope: "" for global or "<host>:<cwd>"), newest first
    pub fn memories(&self, bot_id: &str, scope: Option<&str>) -> Result<Vec<MemoryRow>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT id, bot_id, scope, content, source, updated_at FROM bot_memories
             WHERE bot_id = ?1 AND (?2 IS NULL OR scope = ?2)
             ORDER BY updated_at DESC, id ASC",
        )?;
        let rows = st.query_map(params![bot_id, scope], |r| {
            Ok(MemoryRow {
                id: r.get(0)?,
                bot_id: r.get(1)?,
                scope: r.get(2)?,
                content: r.get(3)?,
                source: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn memory(&self, id: &str) -> Result<MemoryRow> {
        self.db
            .lock()
            .unwrap()
            .query_row(
                "SELECT id, bot_id, scope, content, source, updated_at FROM bot_memories WHERE id = ?1",
                params![id],
                |r| {
                    Ok(MemoryRow {
                        id: r.get(0)?,
                        bot_id: r.get(1)?,
                        scope: r.get(2)?,
                        content: r.get(3)?,
                        source: r.get(4)?,
                        updated_at: r.get(5)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| anyhow!("记忆不存在: {id}"))
    }

    /// Global memories (`scope == ""`) plus memories matching this task's exact canonical workspace (`"<host>:<cwd>"`),
    /// global first then workspace, newest first within each group
    pub fn memories_for_task(&self, bot_id: &str, host: &str, workspace_cwd: &str) -> Result<Vec<MemoryRow>> {
        let ws_key = BotConfig::workspace_key(host, workspace_cwd);
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT id, bot_id, scope, content, source, updated_at FROM bot_memories
             WHERE bot_id = ?1 AND (scope = '' OR (?2 != '' AND scope = ?2))
             ORDER BY CASE WHEN scope = '' THEN 0 ELSE 1 END, updated_at DESC, id ASC",
        )?;
        let rows = st.query_map(params![bot_id, ws_key], |r| {
            Ok(MemoryRow {
                id: r.get(0)?,
                bot_id: r.get(1)?,
                scope: r.get(2)?,
                content: r.get(3)?,
                source: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Insert or update a Bot memory with exact-content deduplication and strict capacity guards (no silent eviction)
    pub fn save_memory(&self, bot_id: &str, id: Option<&str>, scope: &str, content: &str, source: &str, now: i64) -> Result<MemoryRow> {
        let content = content.trim();
        if content.is_empty() {
            bail!("记忆内容不能为空");
        }
        let content: String = if content.chars().count() > MEMORY_MAX_CHARS {
            let mut s: String = content.chars().take(MEMORY_MAX_CHARS).collect();
            s.push('…');
            s
        } else {
            content.to_string()
        };
        let scope = scope.trim().to_string();
        let db = self.db.lock().unwrap();

        if let Some(id) = id.map(str::trim).filter(|s| !s.is_empty()) {
            let existing: Option<(String, String)> = db
                .query_row("SELECT bot_id, scope FROM bot_memories WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            let Some((owner_bot, old_scope)) = existing else {
                bail!("记忆不存在: {id}");
            };
            if owner_bot != bot_id {
                bail!("记忆不存在: {id}");
            }
            if old_scope != scope {
                let scope_count: i64 = db.query_row(
                    "SELECT COUNT(*) FROM bot_memories WHERE bot_id = ?1 AND scope = ?2",
                    params![bot_id, scope],
                    |r| r.get(0),
                )?;
                if scope_count as usize >= MEMORY_MAX_SCOPE {
                    bail!("该作用域的记忆已达上限（{MEMORY_MAX_SCOPE} 条），请先调用 memory_list 并用 memory_delete 合并清理旧记忆。");
                }
            }
            db.execute(
                "UPDATE bot_memories SET scope = ?2, content = ?3, source = ?4, updated_at = ?5 WHERE id = ?1",
                params![id, scope, content, source, now],
            )?;
            return Ok(MemoryRow { id: id.to_string(), bot_id: bot_id.to_string(), scope, content, source: source.to_string(), updated_at: now });
        }

        // Exact deduplication within the same scope: keep the existing id and bump updated_at
        let dup: Option<(String, String)> = db
            .query_row(
                "SELECT id, source FROM bot_memories WHERE bot_id = ?1 AND scope = ?2 AND content = ?3 LIMIT 1",
                params![bot_id, scope, content],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((dup_id, old_source)) = dup {
            // Preserve higher-trust source label (owner > review > task > bot)
            let rank = |s: &str| match s {
                "owner" => 3,
                "review" => 2,
                "task" => 1,
                _ => 0,
            };
            let kept_source = if rank(source) >= rank(&old_source) { source.to_string() } else { old_source };
            db.execute(
                "UPDATE bot_memories SET source = ?2, updated_at = ?3 WHERE id = ?1",
                params![dup_id, kept_source, now],
            )?;
            return Ok(MemoryRow { id: dup_id, bot_id: bot_id.to_string(), scope, content, source: kept_source, updated_at: now });
        }

        let total: i64 = db.query_row("SELECT COUNT(*) FROM bot_memories WHERE bot_id = ?1", params![bot_id], |r| r.get(0))?;
        if total as usize >= MEMORY_MAX_TOTAL {
            bail!("记忆库已达上限（{MEMORY_MAX_TOTAL} 条），请先调用 memory_list 并用 memory_delete 合并或清理过时记忆后再保存。");
        }
        let scope_count: i64 = db.query_row(
            "SELECT COUNT(*) FROM bot_memories WHERE bot_id = ?1 AND scope = ?2",
            params![bot_id, scope],
            |r| r.get(0),
        )?;
        if scope_count as usize >= MEMORY_MAX_SCOPE {
            bail!("该作用域的记忆已达上限（{MEMORY_MAX_SCOPE} 条），请先调用 memory_list 并用 memory_delete 合并或清理过时记忆后再保存。");
        }

        let new_id = format!("mem_{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
        db.execute(
            "INSERT INTO bot_memories (id, bot_id, scope, content, source, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![new_id, bot_id, scope, content, source, now],
        )?;
        Ok(MemoryRow { id: new_id, bot_id: bot_id.to_string(), scope, content, source: source.to_string(), updated_at: now })
    }

    pub fn delete_memory(&self, bot_id: &str, id: &str) -> Result<bool> {
        let n = self
            .db
            .lock()
            .unwrap()
            .execute("DELETE FROM bot_memories WHERE id = ?1 AND bot_id = ?2", params![id, bot_id])?;
        Ok(n > 0)
    }
}

fn put_task(db: &Connection, t: &TaskRow) -> Result<()> {
    db.execute(
        "INSERT INTO tasks (id, bot_id, state, created_at, data) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET state = excluded.state, data = excluded.data",
        params![t.id, t.bot_id, t.state, t.created_at, serde_json::to_string(t)?],
    )?;
    Ok(())
}

fn put_event(db: &Connection, e: &EventRow) -> Result<()> {
    db.execute(
        "UPDATE trigger_events SET disposition = ?2, task_id = ?3, data = ?4 WHERE id = ?1",
        params![e.id, e.disposition, e.task_id, serde_json::to_string(e)?],
    )?;
    Ok(())
}

/// false: the key is already there (the earlier attempt stands)
fn put_outbox(db: &Connection, o: &OutboxRow) -> Result<bool> {
    let n = db.execute(
        "INSERT OR IGNORE INTO outbox (key, instance_id, state, next_at, created_at, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![o.key, o.instance_id, o.state, o.next_at, o.created_at, serde_json::to_string(o)?],
    )?;
    Ok(n > 0)
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
        assert_eq!(s.add_prompt("b1", 0, &Default::default(), &[], "form", 1).unwrap(), 1);
        assert_eq!(s.add_prompt("b1", 1, &Default::default(), &[], "ai", 2).unwrap(), 2);
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
        assert!(s.outbox(None, None, 10).unwrap().is_empty());

        let o2 = OutboxRow { key: "k2".into(), instance_id: "i2".into(), kind: "notify".into(), bot_id: Some("b1".into()), text: "bye".into(), state: "pending".into(), next_at: 5, created_at: 2, ..Default::default() };
        assert!(s.outbox_add(&o2).unwrap());
        assert_eq!(s.outbox(None, Some("b1"), 10).unwrap().len(), 1);
        assert!(s.outbox(None, Some("other"), 10).unwrap().is_empty());
        assert_eq!(s.bot_tasks("b1").unwrap().len(), 1);

        s.save_memory("b1", None, "", "Keep replies brief", "owner", 10).unwrap();
        s.save_memory("b1", None, "local:/repo", "Build on Windows", "task", 11).unwrap();
        assert_eq!(s.memories("b1", None).unwrap().len(), 2);

        s.delete_bot("b1").unwrap();
        assert!(s.bots().unwrap().is_empty());
        assert!(s.tasks(None, 10).unwrap().is_empty());
        assert!(s.bot_tasks("b1").unwrap().is_empty());
        assert!(s.outbox_due(10).unwrap().is_empty());
        assert!(s.memories("b1", None).unwrap().is_empty());
    }

    #[test]
    fn memories_scope_isolation_dedupe_and_capacity_limits() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("bots.db")).unwrap();
        s.save_bot(&bot("b1"), 1).unwrap();

        let m1 = s.save_memory("b1", None, "", "Short Chinese replies", "review", 10).unwrap();
        // Exact dedupe keeps id and upgrades source rank (owner > review)
        let m1_dup = s.save_memory("b1", None, "", "Short Chinese replies", "owner", 20).unwrap();
        assert_eq!(m1.id, m1_dup.id);
        assert_eq!(m1_dup.source, "owner");
        assert_eq!(m1_dup.updated_at, 20);

        // Two hosts with the same folder basename never collide
        s.save_memory("b1", None, "local:/git/app", "Linux pitfall", "task", 30).unwrap();
        s.save_memory("b1", None, "mac:/Users/me/app", "macOS pitfall", "task", 31).unwrap();

        let local_mems = s.memories_for_task("b1", "local", "/git/app").unwrap();
        assert_eq!(local_mems.len(), 2);
        assert_eq!(local_mems[0].scope, "");
        assert_eq!(local_mems[1].content, "Linux pitfall");

        let scratch_mems = s.memories_for_task("b1", "local", "").unwrap();
        assert_eq!(scratch_mems.len(), 1);
        assert_eq!(scratch_mems[0].scope, "");

        // Scope capacity guard refuses silent eviction at MEMORY_MAX_SCOPE
        for i in 1..MEMORY_MAX_SCOPE {
            s.save_memory("b1", None, "", &format!("Rule {i}"), "owner", 100 + i as i64).unwrap();
        }
        assert!(s.save_memory("b1", None, "", "Overflow rule", "owner", 999).is_err());

        // Delete frees a slot
        assert!(s.delete_memory("b1", &m1.id).unwrap());
        assert!(s.save_memory("b1", None, "", "Fits now", "owner", 1000).is_ok());

        // PromptRevision stores sops and preserves backward compatibility with legacy JSON without sops
        let sop = Sop { id: "s1".into(), name: "Bug".into(), when: "bug".into(), how: "fix".into(), ask_human: "plan".into(), delivery: "test".into(), enabled: true };
        let rev = s.add_prompt("b1", 0, &PromptSections { role: "helper".into(), ..Default::default() }, &[sop.clone()], "bot", 50).unwrap();
        assert_eq!(rev, 1);
        let list = s.prompts("b1").unwrap();
        assert_eq!(list[0].sops, vec![sop]);
        assert_eq!(list[0].sections.role, "helper");

        // Active tasks include `reviewing`
        let rt = TaskRow { id: "tr1".into(), bot_id: "b1".into(), state: "reviewing".into(), created_at: 1, ..Default::default() };
        assert!(rt.active());
        s.save_task(&rt).unwrap();
        assert_eq!(s.active_tasks().unwrap().len(), 1);
    }

    #[test]
    fn spawn_writes_task_events_and_ack_together() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("bots.db")).unwrap();
        s.save_bot(&bot("b1"), 1).unwrap();
        let e = EventRow { id: "e1".into(), instance_id: "i".into(), bot_id: "b1".into(), key: "k".into(), at: 5, disposition: "queued".into(), ..Default::default() };
        s.add_event(&e).unwrap();
        let t = TaskRow { id: "t1".into(), bot_id: "b1".into(), state: "queued".into(), created_at: 10, ..Default::default() };
        let spawned = EventRow { disposition: "spawned".into(), task_id: Some("t1".into()), ..e.clone() };
        let ack = OutboxRow { key: "ack:t1".into(), instance_id: "i".into(), kind: "reply".into(), text: "on it".into(), state: "pending".into(), next_at: 10, created_at: 10, ..Default::default() };
        s.spawn(&t, &[spawned], Some(&ack)).unwrap();
        assert_eq!(s.task("t1").unwrap().state, "queued");
        let e = s.event("e1").unwrap();
        assert_eq!((e.disposition.as_str(), e.task_id.as_deref()), ("spawned", Some("t1")));
        assert_eq!(s.outbox_due(10).unwrap().len(), 1);

        // A failing write rolls the whole spawn back
        s.db.lock().unwrap().execute_batch("CREATE TRIGGER no_ack BEFORE INSERT ON outbox BEGIN SELECT RAISE(ABORT, 'no'); END").unwrap();
        let t2 = TaskRow { id: "t2".into(), ..t };
        let ack2 = OutboxRow { key: "ack:t2".into(), ..ack };
        assert!(s.spawn(&t2, &[], Some(&ack2)).is_err());
        assert!(s.task("t2").is_err());
    }
}
