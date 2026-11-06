//! SQLite persistence: session metadata, event log, usage records, queued messages.

use anyhow::Result;
use yi_proto::{Event, EventBody, ModelConfigOption, SessionInfo, Usage};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 info TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS events (
                 session_id TEXT NOT NULL,
                 seq INTEGER NOT NULL,
                 ts INTEGER NOT NULL,
                 body TEXT NOT NULL,
                 PRIMARY KEY (session_id, seq)
             );
             CREATE TABLE IF NOT EXISTS usage_log (
                 session_id TEXT NOT NULL,
                 tool TEXT NOT NULL,
                 ts INTEGER NOT NULL,
                 input_tokens INTEGER NOT NULL,
                 output_tokens INTEGER NOT NULL,
                 cache_read_tokens INTEGER NOT NULL,
                 cache_write_tokens INTEGER NOT NULL,
                 cost_usd REAL NOT NULL,
                 credits REAL NOT NULL
             );
             CREATE INDEX IF NOT EXISTS usage_ts ON usage_log(ts);
             CREATE TABLE IF NOT EXISTS model_configs (
                 model TEXT PRIMARY KEY,
                 options TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS queues (
                 session_id TEXT PRIMARY KEY,
                 items TEXT NOT NULL
             );",
        )?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// Host-wide Cursor parameter snapshots. An empty list is a cached unsupported model, not a miss.
    pub fn model_config(&self, model: &str) -> Result<Option<Vec<ModelConfigOption>>> {
        let raw: Option<String> = self.conn.lock().unwrap().query_row(
            "SELECT options FROM model_configs WHERE model = ?1", [model], |r| r.get(0),
        ).optional()?;
        Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
    }

    pub fn save_model_config(&self, model: &str, options: &[ModelConfigOption]) -> Result<()> {
        // Capabilities are host-wide; selected values belong to a session and must never leak through the cache.
        let options = options.iter().cloned().map(|mut o| { o.current_value.clear(); o }).collect::<Vec<_>>();
        self.conn.lock().unwrap().execute(
            "INSERT OR REPLACE INTO model_configs(model, options) VALUES (?1, ?2)",
            params![model, serde_json::to_string(&options)?],
        )?;
        Ok(())
    }

    /// Find user messages and assistant replies containing q (case-insensitive), newest first
    pub fn search(&self, q: &str, limit: usize) -> Result<Vec<(String, i64, i64, EventBody)>> {
        let c = self.conn.lock().unwrap();
        let pat = format!("%{}%", q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let mut st = c.prepare(
            "SELECT session_id, seq, ts, body FROM events
             WHERE json_extract(body, '$.kind') IN ('user_message', 'assistant_text')
               AND body LIKE ?1 ESCAPE '\\'
             ORDER BY ts DESC LIMIT ?2",
        )?;
        let rows = st.query_map(params![pat, (limit * 3) as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?))
        })?;
        let ql = q.to_lowercase();
        let mut out = vec![];
        for r in rows {
            let (sid, seq, ts, body) = r?;
            let Ok(b) = serde_json::from_str::<EventBody>(&body) else { continue };
            // LIKE matches the raw JSON and may hit escape sequences or field names, so confirm against the text again
            let text = match &b {
                EventBody::UserMessage { text, .. } | EventBody::AssistantText { text, .. } => text,
                _ => continue,
            };
            if !text.to_lowercase().contains(&ql) {
                continue;
            }
            out.push((sid, seq, ts, b));
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    pub fn save_session(&self, info: &SessionInfo) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO sessions(id, info) VALUES(?1, ?2)
             ON CONFLICT(id) DO UPDATE SET info = excluded.info",
            params![info.id, serde_json::to_string(info)?],
        )?;
        Ok(())
    }

    pub fn load_sessions(&self) -> Result<Vec<SessionInfo>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT info FROM sessions")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            if let Ok(info) = serde_json::from_str::<SessionInfo>(&r?) {
                out.push(info);
            }
        }
        Ok(out)
    }

    pub fn delete_session(&self, id: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        c.execute("DELETE FROM events WHERE session_id = ?1", params![id])?;
        c.execute("DELETE FROM queues WHERE session_id = ?1", params![id])?;
        Ok(())
    }

    /// A session's queued messages (JSON array, written whole on every change); an empty queue removes the row
    pub fn save_queue(&self, id: &str, items: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        if items == "[]" {
            c.execute("DELETE FROM queues WHERE session_id = ?1", params![id])?;
        } else {
            c.execute(
                "INSERT INTO queues(session_id, items) VALUES(?1, ?2)
                 ON CONFLICT(session_id) DO UPDATE SET items = excluded.items",
                params![id, items],
            )?;
        }
        Ok(())
    }

    /// All saved queues: (session id, JSON array)
    pub fn load_queues(&self) -> Result<Vec<(String, String)>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT session_id, items FROM queues")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Delete all of a session's events (/clear); the session itself is kept
    pub fn delete_events(&self, id: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM events WHERE session_id = ?1", params![id])?;
        Ok(())
    }

    pub fn append_event(&self, session_id: &str, ev: &Event) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT OR REPLACE INTO events(session_id, seq, ts, body) VALUES(?1, ?2, ?3, ?4)",
            params![session_id, ev.seq, ev.ts, serde_json::to_string(&ev.body)?],
        )?;
        Ok(())
    }

    /// Events with seq > since, at most limit (ascending by seq)
    pub fn events_since(&self, session_id: &str, since: i64, limit: usize) -> Result<Vec<Event>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(
            "SELECT seq, ts, body FROM events WHERE session_id = ?1 AND seq > ?2
             ORDER BY seq ASC LIMIT ?3",
        )?;
        let rows = st.query_map(params![session_id, since, limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
        })?;
        let mut out = vec![];
        for r in rows {
            let (seq, ts, body) = r?;
            if let Ok(body) = serde_json::from_str::<EventBody>(&body) {
                out.push(Event { seq, ts, body });
            }
        }
        Ok(out)
    }

    /// The last limit events with seq < before (ascending by seq), and whether there are older ones. before=None means the latest
    pub fn events_tail(
        &self,
        session_id: &str,
        before: Option<i64>,
        limit: usize,
    ) -> Result<(Vec<Event>, bool)> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(
            "SELECT seq, ts, body FROM events WHERE session_id = ?1 AND seq < ?2
             ORDER BY seq DESC LIMIT ?3",
        )?;
        let rows = st.query_map(
            params![session_id, before.unwrap_or(i64::MAX), limit as i64 + 1],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)),
        )?;
        let mut raw = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let more = raw.len() > limit;
        raw.truncate(limit);
        let mut out: Vec<Event> = raw
            .into_iter()
            .rev()
            .filter_map(|(seq, ts, body)| {
                serde_json::from_str::<EventBody>(&body).ok().map(|body| Event { seq, ts, body })
            })
            .collect();
        out.sort_by_key(|e| e.seq);
        Ok((out, more))
    }

    /// Number of user messages recorded in the session
    pub fn count_user_messages(&self, session_id: &str) -> Result<u32> {
        let c = self.conn.lock().unwrap();
        let n: i64 = c.query_row(
            "SELECT COUNT(*) FROM events WHERE session_id = ?1 AND body LIKE '{\"kind\":\"user_message\"%'",
            params![session_id],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    pub fn max_seq(&self, session_id: &str) -> Result<i64> {
        let c = self.conn.lock().unwrap();
        let v: Option<i64> = c
            .query_row(
                "SELECT MAX(seq) FROM events WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(v.unwrap_or(0))
    }

    pub fn log_usage(&self, session_id: &str, tool: &str, ts: i64, u: &Usage) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO usage_log VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                session_id,
                tool,
                ts,
                u.input_tokens as i64,
                u.output_tokens as i64,
                u.cache_read_tokens as i64,
                u.cache_write_tokens as i64,
                u.cost_usd,
                u.credits
            ],
        )?;
        Ok(())
    }

    /// Returns (ts, tool, usage) records for daily aggregation in stats
    pub fn usage_rows(&self, since_ms: i64) -> Result<Vec<(i64, String, Usage)>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(
            "SELECT ts, tool, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    cost_usd, credits FROM usage_log WHERE ts >= ?1 ORDER BY ts",
        )?;
        let rows = st.query_map(params![since_ms], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                Usage {
                    input_tokens: r.get::<_, i64>(2)? as u64,
                    output_tokens: r.get::<_, i64>(3)? as u64,
                    cache_read_tokens: r.get::<_, i64>(4)? as u64,
                    cache_write_tokens: r.get::<_, i64>(5)? as u64,
                    cost_usd: r.get(6)?,
                    credits: r.get(7)?,
                },
            ))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_proto::*;

    #[test]
    fn model_config_cache_survives_reopen_and_distinguishes_empty_from_missing() {
        let path = std::env::temp_dir().join(format!("yi-model-cache-{}.db", uuid::Uuid::new_v4()));
        {
            let store = Store::open(&path).unwrap();
            store.save_model_config("no-parameters", &[]).unwrap();
        }
        {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.model_config("no-parameters").unwrap(), Some(vec![]));
            assert_eq!(store.model_config("unknown").unwrap(), None);
        }
        let _ = std::fs::remove_file(path);
    }

    fn info(id: &str) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            tool: ToolKind::Claude,
            mode: SessionMode::Structured,
            cwd: "/tmp".into(),
            title: "t".into(),
            status: SessionStatus::Idle,
            external_id: None,
            created_at: 1,
            updated_at: 1,
            last_seq: 0,
            pending_permissions: 0,
            usage: Usage::default(),
            model: None,
            command: None,
            context_percent: None,
            permission_mode: None,
            effort: None,
            model_config_options: vec![],
            model_config_pending: false,
            pinned: false,
            parent_id: None,
            synced_users: None,
            auto_resume: None,
            limit_hits: 0,
            computer_use: false,
            queued: vec![],
            queue_paused: None,
            restart_interrupted: false,
            background_tasks: vec![],
        }
    }

    #[test]
    fn clear_keeps_session_and_seq() {
        let s = Store::open_memory().unwrap();
        s.save_session(&info("a")).unwrap();
        s.save_session(&info("b")).unwrap();
        for (sid, seq) in [("a", 1), ("a", 2), ("b", 1)] {
            let body = EventBody::UserMessage { text: "m".into(), images: vec![] };
            s.append_event(sid, &Event { seq, ts: seq, body }).unwrap();
        }
        s.delete_events("a").unwrap();
        assert!(s.events_since("a", 0, 100).unwrap().is_empty());
        assert_eq!(s.events_since("b", 0, 100).unwrap().len(), 1);
        assert_eq!(s.load_sessions().unwrap().len(), 2);
        // The clear marker continues the existing seq numbering; clients drop earlier events based on it
        s.append_event("a", &Event { seq: 3, ts: 3, body: EventBody::Cleared {} }).unwrap();
        let ev = s.events_since("a", 0, 100).unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].seq, 3);
        assert_eq!(serde_json::to_string(&ev[0].body).unwrap(), r#"{"kind":"cleared"}"#);
    }

    #[test]
    fn events_and_sessions() {
        let s = Store::open_memory().unwrap();
        s.save_session(&info("a")).unwrap();
        for i in 1..=5 {
            s.append_event(
                "a",
                &Event {
                    seq: i,
                    ts: i,
                    body: EventBody::UserMessage {
                        text: format!("m{i}"),
                        images: vec![],
                    },
                },
            )
            .unwrap();
        }
        assert_eq!(s.max_seq("a").unwrap(), 5);
        assert_eq!(s.count_user_messages("a").unwrap(), 5);
        assert_eq!(s.count_user_messages("b").unwrap(), 0);
        let (tail, more) = s.events_tail("a", None, 2).unwrap();
        assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![4, 5]);
        assert!(more);
        let (older, more) = s.events_tail("a", Some(4), 10).unwrap();
        assert_eq!(older.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert!(!more);
        let ev = s.events_since("a", 3, 100).unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].seq, 4);
        assert_eq!(s.load_sessions().unwrap().len(), 1);
        s.append_event("a", &Event { seq: 6, ts: 6, body: EventBody::AssistantText { id: "x".into(), text: "修复 Nginx 配置 100%".into() } }).unwrap();
        let hits = s.search("nginx", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, 6);
        assert_eq!(s.search("100%", 10).unwrap().len(), 1);
        assert_eq!(s.search("m3", 10).unwrap().len(), 1);
        assert_eq!(s.search("kind", 10).unwrap().len(), 0);
        s.delete_session("a").unwrap();
        assert_eq!(s.max_seq("a").unwrap(), 0);
    }
}
