//! MCP tools the desktop app serves to Bot sessions (`yi_bot`) and task sessions (`yi_task`) through the hosts'
//! client MCP bridge.

use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const BOT_SERVER: &str = "yi_bot";
pub const TASK_SERVER: &str = "yi_task";

/// Marks results given while the app was offline, so the engine can find and replay those calls later
pub const OFFLINE_MARK: &str = "[yi-bots:offline]";

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": properties, "required": required},
    })
}

fn ids() -> Value {
    json!({"type": "array", "items": {"type": "string"}})
}

pub fn bot_tools() -> Vec<Value> {
    vec![
        tool(
            "spawn_task",
            "Start a task session for events. The original events are attached automatically; do not restate them.",
            json!({
                "event_ids": {"type": "array", "items": {"type": "string"}, "description": "Events this task handles; may be empty when the owner asked in this session directly"},
                "title": {"type": "string", "description": "Short title shown in the sidebar"},
                "request": {"type": "string", "description": "Only when the owner asked in this session directly: their request, quoted as they wrote it"},
                "note": {"type": "string", "description": "At most 30 words of routing information; never an implementation plan"},
                "workspace": {"type": "integer", "description": "Workspace index (default 0)"},
                "tool": {"type": "string", "description": "Task tool, one of those listed under Environment; omit for the default"},
                "model": {"type": "string"},
                "worktree": {"type": "boolean", "description": "false to work directly in the workspace directory"},
            }),
            &["title"],
        ),
        tool(
            "attach",
            "Hand an event to a task that is already working on its conversation.",
            json!({"event_id": {"type": "string"}, "task_id": {"type": "string"}}),
            &["event_id", "task_id"],
        ),
        tool("ignore", "Drop events that need no action.", json!({"event_ids": ids(), "reason": {"type": "string"}}), &["event_ids", "reason"]),
        tool(
            "send_task",
            "Pass extra information to a running task, or changes asked for to a ready one (it goes back to work). Grants no new permissions.",
            json!({"task_id": {"type": "string"}, "text": {"type": "string"}}),
            &["task_id", "text"],
        ),
        tool("task_status", "State, last report and session of one task.", json!({"task_id": {"type": "string"}}), &["task_id"]),
        tool(
            "list_tasks",
            "This Bot's tasks, newest first.",
            json!({"active_only": {"type": "boolean", "description": "Only open tasks: queued / working / needsYou / ready (default true)"}}),
            &[],
        ),
        tool("stop_task", "Stop a task.", json!({"task_id": {"type": "string"}, "reason": {"type": "string"}}), &["task_id"]),
        tool(
            "close_task",
            "Close a task whose turn has ended (working, blocked or ready; not running) when its outcome is settled: after relaying a ready result that needs no review by the owner, or after a waiting report that already holds the result.",
            json!({
                "task_id": {"type": "string"},
                "result": {"type": "string", "enum": ["done", "failed"]},
                "summary": {"type": "string", "description": "One sentence on the outcome, shown on the task"},
            }),
            &["task_id", "result", "summary"],
        ),
        tool(
            "reply",
            "Answer in an event's conversation, through the plugin it came from.",
            json!({"event_id": {"type": "string"}, "text": {"type": "string"}}),
            &["event_id", "text"],
        ),
        tool(
            "notify_human",
            "Ask the owner for something only they can do. Shows up as pending in the app and goes to the notify channels.",
            json!({"text": {"type": "string"}, "task_id": {"type": "string"}, "urgency": {"type": "string", "enum": ["low", "normal", "high"]}}),
            &["text"],
        ),
        tool("read_event", "Full text of an event.", json!({"event_id": {"type": "string"}}), &["event_id"]),
        tool(
            "read_thread",
            "Earlier messages of an event's conversation, from the plugin, page by page.",
            json!({"event_id": {"type": "string"}, "cursor": {"type": "string"}}),
            &["event_id"],
        ),
        tool("plugin_tools", "Tools offered by the plugin instances this Bot uses.", json!({}), &[]),
        tool(
            "plugin_call",
            "Run a plugin tool listed by plugin_tools.",
            json!({
                "instance": {"type": "string"},
                "name": {"type": "string"},
                "input": {"type": "object"},
                "summary": {"type": "string", "description": "One sentence on what the call does, shown to the owner if it needs approval"},
            }),
            &["instance", "name"],
        ),
        tool(
            "routine_done",
            "Close a routine run: result nothing (nothing worth reporting), done (with a one or two sentence summary) or failed (with why).",
            json!({
                "run_id": {"type": "string"},
                "result": {"type": "string", "enum": ["done", "nothing", "failed"]},
                "summary": {"type": "string"},
            }),
            &["run_id", "result"],
        ),
        tool("notes_read", "Your durable notes.", json!({}), &[]),
        tool("notes_write", "Replace your durable notes (keep them short).", json!({"text": {"type": "string"}}), &["text"]),
        tool("get_config", "Your current configuration: name, workspaces, tools, triggers, notify, accept, allow list, budget, prompt.", json!({}), &[]),
        tool(
            "update_config",
            "Change your configuration when the owner asks for it in this session. `changes` is a JSON merge patch over \
             get_config: objects merge, anything else (arrays included) replaces, so pass arrays in full. Instruction \
             changes are saved as a prompt revision the owner can restore and apply from your next turn. Changing accept \
             to auto, the allow list or a permission mode waits for the owner to confirm in the app.",
            json!({
                "changes": {"type": "object", "description": "Fields to change, shaped like get_config"},
                "instances": new_instances(),
                "reason": {"type": "string", "description": "One sentence for the owner, in their language: what changes and why"},
            }),
            &["changes", "reason"],
        ),
        tool("list_hosts", "Hosts of the owner's desktop app: id, name, connected, home directory.", json!({}), &[]),
        tool("list_tools", "AI tools installed on a host (only those that can run a Bot or a task).", json!({"host": {"type": "string"}}), &["host"]),
        tool(
            "list_models",
            "Models a tool offers on a host.",
            json!({"host": {"type": "string"}, "tool": {"type": "string"}}),
            &["host", "tool"],
        ),
        tool("list_plugins", "Installed plugins: capabilities, permissions and the JSON schema of their configuration.", json!({}), &[]),
        tool("list_instances", "Configured plugin instances. Secret values show as ********.", json!({}), &[]),
        tool(
            "request_secret",
            "Ask the owner to enter or replace secrets (tokens, keys) of a plugin instance you use, e.g. when its token \
             was refused. They get a card in this session; the value goes straight into the configuration and you never see it.",
            json!({
                "instance_id": {"type": "string"},
                "fields": {"type": "array", "items": {"type": "string"}, "description": "Secret settings to ask for; default all of them"},
                "reason": {"type": "string", "description": "One sentence for the owner, in their language: why it is needed"},
            }),
            &["instance_id", "reason"],
        ),
        tool(
            "propose_plugin",
            "Submit a plugin a task built, after you checked it, for the owner to review and install: a .yiplugin file or a \
             folder with plugin.json and the component, on the task's host (relative paths start at the task's directory). \
             Nothing is installed until the owner confirms its permissions.",
            json!({"task_id": {"type": "string"}, "path": {"type": "string"}}),
            &["task_id", "path"],
        ),
    ]
}

/// Bot tools that only look things up for a configuration change
pub const SETUP_TOOLS: [&str; 5] = ["list_hosts", "list_tools", "list_models", "list_plugins", "list_instances"];

pub fn task_tools() -> Vec<Value> {
    vec![
        tool(
            "report",
            "Tell the Bot where the task is.",
            json!({
                "state": {"type": "string", "enum": ["progress", "ready", "blocked", "failed", "dropped"]},
                "summary": {"type": "string", "description": "One or two sentences"},
                "question": {"type": "string", "description": "blocked: the one concrete question for the person"},
                "links": {"type": "array", "items": {"type": "string"}},
            }),
            &["state", "summary"],
        ),
        tool(
            "ack",
            "Confirm once before starting: the object and the expected result, in one sentence.",
            json!({"text": {"type": "string"}, "event_id": {"type": "string"}}),
            &["text"],
        ),
        tool(
            "handback",
            "A follow-up asks for something outside this task: give it back to the Bot.",
            json!({"event_id": {"type": "string"}, "reason": {"type": "string"}}),
            &["reason"],
        ),
        tool("read_event", "Full text of an event.", json!({"event_id": {"type": "string"}}), &["event_id"]),
        tool(
            "read_thread",
            "Earlier messages of an event's conversation, page by page.",
            json!({"event_id": {"type": "string"}, "cursor": {"type": "string"}}),
            &["event_id"],
        ),
        tool(
            "plugin_kit",
            "Only for a task that builds a plugin: write the plugin SDK, the WIT interface, an example plugin and the \
             author guide into a folder, as a cargo workspace to build the plugin in.",
            json!({"dir": {"type": "string", "description": "Target folder (default: yi-plugin-kit in this task's directory)"}}),
            &[],
        ),
    ]
}

/// JSON schema of a Bot configuration (`config::BotConfig`) as a Bot changes it. `validate` also rejects keys
/// that are not in here (`unknown_keys`), so a test keeps it in step with the struct
pub fn draft() -> Value {
    let s = |d: &str| json!({"type": "string", "description": d});
    let strings = |d: &str| json!({"type": "array", "items": {"type": "string"}, "description": d});
    let tool = json!({"type": "string", "enum": crate::config::AI_TOOLS});
    json!({
        "type": "object",
        "description": "A Bot configuration (get_bot returns the same shape). Leave id empty for a new Bot. Plugin \
                        instances to create are referenced by their ref (\"new:...\") in triggers, notify and allow. \
                        Only the fields below exist; validate rejects any other.",
        "additionalProperties": false,
        "required": ["name", "bot"],
        "properties": {
            "id": s("Empty for a new Bot; the Bot's id when changing one"),
            "name": s("Display name, at most 40 characters"),
            "enabled": {"type": "boolean", "description": "Leave false: the owner enables the Bot after reviewing it"},
            "workspaces": {
                "type": "array",
                "description": "Directories where tasks run; the first is the default. Without one the Bot cannot start tasks",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["host", "cwd"],
                    "properties": {"host": s("Host id (list_hosts)"), "cwd": s("Directory on that host, usually a git repository")},
                },
            },
            "bot": {
                "type": "object",
                "description": "The Bot's own always-on session",
                "additionalProperties": false,
                "required": ["host", "tool"],
                "properties": {
                    "host": s("Host id (list_hosts)"),
                    "cwd": s("Leave empty: ~/.yi-console/bots/<bot id> on that host"),
                    "tool": tool.clone(),
                    "model": s("Model id (list_models); omit for the tool's default"),
                    "effort": s("Reasoning effort; omit for the default"),
                    "permissionMode": s("Omit for the tool's default"),
                },
            },
            "task": {
                "type": "object",
                "description": "How tasks run",
                "additionalProperties": false,
                "properties": {
                    "tool": tool.clone(),
                    "tools": {"type": "array", "items": tool.clone(), "description": "Other tools the Bot may pick per task; say in prompt.role which suits what"},
                    "model": s("Model for the default tool; omit for its default"),
                    "effort": s("Reasoning effort for the default tool"),
                    "permissionMode": {"type": "string", "description": "Default bypass (tasks run unattended)"},
                    "worktree": {"type": "boolean", "description": "One git worktree per task (default true)"},
                    "maxConcurrent": {"type": "integer", "description": "Tasks running at once (default 3)"},
                    "timeoutMinutes": {"type": "integer", "description": "Per task; 0 means none (default 120)"},
                    "keepDays": {"type": "integer", "description": "Days an ended task keeps its session before it is cleaned up; 0 means forever (default 7)"},
                },
            },
            "triggers": strings("Plugin instance ids or new: refs whose events the Bot handles"),
            "notify": strings("Where reminders go: \"app\" (desktop notification) and/or instance ids / new: refs with the notify capability"),
            "mcp": {
                "type": "object",
                "additionalProperties": false,
                "properties": {"bot": strings("MCP server names for the Bot session"), "task": strings("MCP server names for tasks")},
            },
            "accept": {"type": "string", "enum": ["off", "observe", "auto"], "description": "off: events are only recorded; observe (default for new Bots): the Bot decides but only records what it would do; auto: carried out"},
            "allow": {
                "type": "array",
                "description": "Senders whose explicit requests run in auto mode, per instance",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["instance"],
                    "properties": {"instance": s("Instance id or new: ref"), "ids": strings("Sender ids on that instance")},
                },
            },
            "budget": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "tasksPerDay": {"type": "integer", "description": "Tasks started per day; 0 means no limit"},
                    "tokensPerDay": {"type": "integer", "description": "Tokens per day (the Bot plus its tasks) before the owner gets one reminder; nothing is paused. 0 means no reminder"},
                },
            },
            "routines": {
                "type": "array",
                "description": "Things the Bot does on a schedule (a daily summary, a weekly check). The whole list is replaced on change; keep the ids of existing ones",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "schedule", "prompt"],
                    "properties": {
                        "id": s("Keep for existing routines; leave empty for new ones"),
                        "name": s("Short name"),
                        "schedule": s("Five-field cron in local time, e.g. \"0 9 * * 1-5\" for 09:00 on weekdays"),
                        "prompt": s("What to do on each run, in the owner's words"),
                        "enabled": {"type": "boolean", "description": "Default true"},
                    },
                },
            },
            "approval": {
                "type": "object",
                "description": "In auto mode, outgoing actions from turns the owner didn't start become drafts the owner approves (true for new Bots)",
                "additionalProperties": false,
                "properties": {
                    "reply": {"type": "boolean", "description": "Replies into conversations"},
                    "write": {"type": "boolean", "description": "Plugin tool calls that change something"},
                },
            },
            "activeHours": {
                "type": ["object", "null"],
                "description": "Plugin events outside these hours wait until they start (routines, task reports and the owner are not affected); null: always active",
                "additionalProperties": false,
                "required": ["start", "end"],
                "properties": {
                    "days": {"type": "array", "items": {"type": "integer"}, "description": "ISO weekdays, 1 = Monday; empty: every day"},
                    "start": s("HH:MM"),
                    "end": s("HH:MM; before start means it spans midnight"),
                },
            },
            "prompt": {
                "type": "object",
                "description": "The owner's sections of the Bot's prompt, in their language",
                "additionalProperties": false,
                "properties": {
                    "role": s("What the Bot looks after, how it decides, where it stops and asks"),
                    "communication": s("When and how it speaks to people"),
                    "delivery": s("What a finished task result must include (also given to every task)"),
                },
            },
            "promptRevision": {"type": "integer", "description": "Keep what get_bot returned; 0 for a new Bot"},
            "compactAt": {"type": "integer", "description": "Compact the Bot session at this context percentage (default 70)"},
        },
    })
}

/// Keys in `v` that `schema` does not declare, as dotted paths (objects with `additionalProperties: false` only)
pub fn unknown_keys(schema: &Value, v: &Value, path: &str, out: &mut Vec<String>) {
    if let (Some(props), Some(obj)) = (schema["properties"].as_object(), v.as_object()) {
        for (k, x) in obj {
            let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
            match props.get(k) {
                Some(sub) => unknown_keys(sub, x, &p, out),
                None if schema["additionalProperties"] == false => out.push(p),
                None => {}
            }
        }
    }
    if let (Some(items), Some(arr)) = (schema.get("items"), v.as_array()) {
        for (i, x) in arr.iter().enumerate() {
            unknown_keys(items, x, &format!("{path}[{i}]"), out);
        }
    }
}

fn new_instances() -> Value {
    json!({
        "type": "array",
        "description": "Plugin instances to create along with the change. Secret fields stay empty: the owner fills them in.",
        "items": {
            "type": "object",
            "properties": {
                "ref": {"type": "string", "description": "\"new:\" plus a short name, used in triggers / notify / allow in place of the id"},
                "pluginId": {"type": "string"},
                "name": {"type": "string"},
                "config": {"type": "object", "description": "Values for the plugin's config schema, secrets left out"},
            },
            "required": ["ref", "pluginId", "config"],
        },
    })
}

/// Files of the plugin kit (the task tool `plugin_kit`): a cargo workspace with the SDK and an example, relative path → content
pub fn plugin_kit() -> Vec<(&'static str, String)> {
    let example = include_str!("../../../plugins/testkit/Cargo.toml")
        .replace("name = \"yi-plugin-testkit\"", "name = \"yi-plugin-example\"")
        .replace(
            "Yi Console plugin used by the end-to-end tests: talks to a local mock server and exercises the sandbox",
            "Example plugin: polls a chat server, replies, offers tools",
        );
    vec![
        (
            "Cargo.toml",
            "# Yi Console plugin kit. Build a plugin: cargo build --release --target wasm32-wasip2 -p <crate>\n\
             # (once: rustup target add wasm32-wasip2). Add new plugins to `members`.\n\
             [workspace]\nresolver = \"2\"\nmembers = [\"sdk\", \"example\"]\n\n\
             [workspace.package]\nversion = \"0.1.0\"\nedition = \"2021\"\nlicense = \"MIT\"\n\n\
             [workspace.dependencies]\nyi-plugin = { path = \"sdk\" }\n\
             wit-bindgen = { version = \"0.63\", default-features = false, features = [\"macros\", \"realloc\", \"std\", \"bitflags\"] }\n\
             serde = { version = \"1\", features = [\"derive\"] }\nserde_json = \"1\"\n\n\
             [profile.release]\nopt-level = \"s\"\nlto = true\nstrip = true\ncodegen-units = 1\n"
                .to_string(),
        ),
        ("GUIDE.md", include_str!("../../../docs/plugins.en.md").to_string()),
        ("wit/yi-plugin.wit", include_str!("../../../plugins/wit/yi-plugin.wit").to_string()),
        ("sdk/Cargo.toml", include_str!("../../../plugins/sdk/Cargo.toml").to_string()),
        ("sdk/src/lib.rs", include_str!("../../../plugins/sdk/src/lib.rs").to_string()),
        ("example/Cargo.toml", example),
        ("example/plugin.json", include_str!("../../../plugins/testkit/plugin.json").replace("\"testkit\"", "\"example\"")),
        ("example/prompt.md", include_str!("../../../plugins/testkit/prompt.md").to_string()),
        ("example/src/lib.rs", include_str!("../../../plugins/testkit/src/lib.rs").to_string()),
    ]
}

/// What tasks hear while the app is offline: their reports are kept in the session and replayed later
pub fn task_offline() -> BTreeMap<String, String> {
    ["report", "ack", "handback"]
        .into_iter()
        .map(|t| (t.to_string(), format!("Recorded; the Bot will handle it when the desktop app is back. {OFFLINE_MARK}")))
        .collect()
}

pub fn client_mcp(server: &str, client_id: &str) -> Value {
    let (tools, offline) = match server {
        BOT_SERVER => (bot_tools(), BTreeMap::new()),
        _ => (task_tools(), task_offline()),
    };
    json!({"name": server, "clientId": client_id, "tools": tools, "offline": offline})
}

/// The name an AI tool records for an MCP tool call, back to (server, tool): Claude `mcp__yi_task__report`,
/// Codex and ACP tools `yi_task.report`, `yi_task/report` or `yi_task__report`
pub fn parse_call_name(name: &str) -> Option<(&str, &str)> {
    let n = name.strip_prefix("mcp__").unwrap_or(name);
    for server in [BOT_SERVER, TASK_SERVER] {
        if let Some(rest) = n.strip_prefix(server) {
            let tool = rest.trim_start_matches(['.', '/', '_', ':']);
            if !tool.is_empty() && rest.len() > tool.len() {
                return Some((server, tool));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_from_every_ai_tool() {
        assert_eq!(parse_call_name("mcp__yi_task__report"), Some(("yi_task", "report")));
        assert_eq!(parse_call_name("yi_task.report"), Some(("yi_task", "report")));
        assert_eq!(parse_call_name("yi_bot/spawn_task"), Some(("yi_bot", "spawn_task")));
        assert_eq!(parse_call_name("Bash"), None);
        assert_eq!(parse_call_name("yi_task"), None);
    }

    #[test]
    fn only_task_reports_are_accepted_offline() {
        let v = client_mcp(TASK_SERVER, "c");
        assert!(v["offline"]["report"].as_str().unwrap().contains(OFFLINE_MARK));
        assert!(v["offline"].get("read_event").is_none());
        assert!(client_mcp(BOT_SERVER, "c")["offline"].as_object().unwrap().is_empty());
    }

    #[test]
    fn bots_change_their_config_but_only_propose_plugins() {
        let names = |v: Vec<Value>| v.iter().map(|t| t["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let bot = names(bot_tools());
        assert!(["get_config", "update_config", "propose_plugin"].iter().all(|n| bot.iter().any(|b| b == n)));
        assert!(SETUP_TOOLS.iter().all(|n| bot.iter().any(|b| b == n)));
        assert!(!bot.iter().any(|n| n.contains("install") || n.contains("delete") || n == "plugin_kit"));
        // Building a plugin is a task's work
        assert!(names(task_tools()).iter().any(|n| n == "plugin_kit"));
    }

    #[test]
    fn the_draft_schema_covers_every_config_field() {
        // Every field filled, so serialization shows them all
        let mut c: crate::config::BotConfig = serde_json::from_value(json!({
            "name": "x",
            "workspaces": [{"host": "local", "cwd": "/repo"}],
            "bot": {"host": "local", "tool": "claude", "model": "m", "effort": "high", "permissionMode": "default"},
            "task": {"tools": ["codex"], "model": "m", "effort": "high"},
            "allow": [{"instance": "i", "ids": ["a"]}],
            "mcp": {"bot": ["a"], "task": ["b"]},
            "routines": [{"name": "r", "schedule": "0 9 * * *", "prompt": "p"}],
            "activeHours": {"days": [1], "start": "09:00", "end": "18:00"},
        }))
        .unwrap();
        c.normalize();
        let full = serde_json::to_value(&c).unwrap();
        let mut unknown = vec![];
        unknown_keys(&draft(), &full, "", &mut unknown);
        assert!(unknown.is_empty(), "missing from tools::draft(): {unknown:?}");
        // Guesses an AI made in practice are named
        let guess = json!({"name": "x", "host": "local", "role": "r", "bot": {"host": "local", "tool": "claude", "workdir": "/x"}, "workspaces": [{"host": "local", "cwd": "/r", "dir": "/r"}]});
        let mut unknown = vec![];
        unknown_keys(&draft(), &guess, "", &mut unknown);
        assert_eq!(unknown, ["bot.workdir", "host", "role", "workspaces[0].dir"]);
    }

    #[test]
    fn plugin_kit_is_a_workspace_of_its_own() {
        let kit = plugin_kit();
        let get = |p: &str| kit.iter().find(|(k, _)| *k == p).map(|(_, v)| v.clone()).unwrap();
        assert!(get("Cargo.toml").contains("members = [\"sdk\", \"example\"]"));
        assert!(get("example/Cargo.toml").contains("name = \"yi-plugin-example\""));
        assert!(get("example/plugin.json").contains("\"id\": \"example\""));
        // The SDK finds the WIT next to it, as in the repository
        assert!(get("sdk/src/lib.rs").contains("path: \"../wit\""));
        assert!(get("wit/yi-plugin.wit").contains("package yi:plugin"));
    }
}
