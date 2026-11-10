//! GitHub plugin: polls the notifications API, turns them into Bot events, replies as issue / PR comments, reads a
//! conversation's comments as context, and offers a read-only REST tool.

use serde::Deserialize;
use serde_json::{json, Value};
use yi_plugin::{cut, emit, http, kv_get, kv_set, ContextMessage, ContextPage, Event, Plugin, SendResult, Sender, ToolDef};

const API: &str = "https://api.github.com";
const MAX_TEXT: usize = 4000;

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Config {
    token: String,
    #[serde(default)]
    repos: Vec<String>,
    #[serde(default)]
    poll_seconds: Option<u64>,
}

fn config() -> Result<Config, String> {
    let c: Config = yi_plugin::config()?;
    if c.token.trim().is_empty() {
        return Err("Access token is missing".into());
    }
    Ok(c)
}

fn api(method: &str, path: &str, token: &str) -> http::Request {
    let url = if path.starts_with("https://") { path.to_string() } else { format!("{API}{path}") };
    http::request(method, url)
        .bearer(token)
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", "2022-11-28")
        .header("user-agent", "yi-console-plugin")
}

/// The token's own login, cached: its own comments are not events
fn me(token: &str) -> Result<String, String> {
    if let Some(l) = kv_get("login") {
        return Ok(l);
    }
    let r = api("GET", "/user", token).send()?;
    if r.status == 401 {
        return Err("The access token is invalid or has expired".into());
    }
    let v: Value = r.expect_json()?;
    let login = v["login"].as_str().unwrap_or("").to_string();
    kv_set("login", &login);
    Ok(login)
}

/// "github:owner/repo#12" → ("owner/repo", 12)
pub fn parse_thread(thread: &str) -> Option<(String, u64)> {
    let rest = thread.strip_prefix("github:")?;
    let (repo, n) = rest.rsplit_once('#')?;
    if repo.split('/').count() != 2 {
        return None;
    }
    Some((repo.to_string(), n.parse().ok()?))
}

/// The issue / PR number at the end of a subject URL
pub fn number_of(url: &str) -> Option<u64> {
    url.rsplit('/').next()?.parse().ok()
}

/// Reasons that mean "this is for you"
pub fn addressed(reason: &str) -> bool {
    matches!(reason, "mention" | "team_mention" | "assign" | "review_requested")
}

/// One notification as an event (None: skip it). `comment` is the latest comment (or the issue itself)
pub fn to_event(n: &Value, comment: &Value, me: &str) -> Option<Event> {
    let repo = n["repository"]["full_name"].as_str()?;
    let subject = &n["subject"];
    let kind = subject["type"].as_str().unwrap_or("");
    let title = subject["title"].as_str().unwrap_or("");
    let reason = n["reason"].as_str().unwrap_or("");
    let key = format!("{}:{}", n["id"].as_str().unwrap_or(""), n["updated_at"].as_str().unwrap_or(""));
    let (thread, number) = match kind {
        "Issue" | "PullRequest" => {
            let num = number_of(subject["url"].as_str().unwrap_or(""))?;
            (format!("github:{repo}#{num}"), Some(num))
        }
        "CheckSuite" => (format!("github:{repo}:ci"), None),
        _ => (format!("github:{repo}:{}", kind.to_ascii_lowercase()), None),
    };
    let user = &comment["user"];
    let login = user["login"].as_str().unwrap_or("");
    if !login.is_empty() && login.eq_ignore_ascii_case(me) {
        return None;
    }
    let body = comment["body"].as_str().unwrap_or("");
    let label = match (kind, number) {
        ("PullRequest", Some(n)) => format!("PR #{n}"),
        ("Issue", Some(n)) => format!("Issue #{n}"),
        _ => kind.to_string(),
    };
    let text = if body.is_empty() { format!("[{repo} {label}] {title} ({reason})") } else { format!("[{repo} {label}] {title} ({reason})\n\n{body}") };
    let url = comment["html_url"].as_str().map(str::to_string);
    let sender_id = if login.is_empty() { "github".to_string() } else { login.to_string() };
    Some(
        Event::new(key, thread, cut(&text, MAX_TEXT))
            .sender(sender_id, None, user["type"] == "Bot" || login.ends_with("[bot]"))
            .addressed(addressed(reason) && kind != "CheckSuite")
            .url(url)
            .meta(json!({"reason": reason, "type": kind, "repo": repo})),
    )
}

fn poll() -> Result<u64, String> {
    let c = config()?;
    let me = me(&c.token)?;
    let mut req = api("GET", "/notifications?participating=true&per_page=50", &c.token);
    if let Some(lm) = kv_get("lastModified") {
        req = req.header("if-modified-since", lm);
    }
    let r = req.send()?;
    let interval = r.header("x-poll-interval").and_then(|s| s.parse().ok()).unwrap_or(60u64);
    match r.status {
        304 => return Ok(interval),
        401 => return Err("The access token is invalid or has expired".into()),
        _ => {}
    }
    if let Some(lm) = r.header("last-modified") {
        kv_set("lastModified", lm);
    }
    let list: Vec<Value> = r.expect_json()?;
    for n in list.iter().rev() {
        let repo = n["repository"]["full_name"].as_str().unwrap_or("");
        if !c.repos.is_empty() && !c.repos.iter().any(|x| x.trim().eq_ignore_ascii_case(repo)) {
            continue;
        }
        let latest = n["subject"]["latest_comment_url"].as_str().or(n["subject"]["url"].as_str()).unwrap_or("");
        let comment: Value = if latest.is_empty() {
            Value::Null
        } else {
            api("GET", latest, &c.token).send().ok().filter(|r| r.ok()).and_then(|r| r.json().ok()).unwrap_or(Value::Null)
        };
        if let Some(e) = to_event(n, &comment, &me) {
            emit(e);
        }
        // Read: the next poll only brings what is new
        if let Some(id) = n["id"].as_str() {
            let _ = api("PATCH", &format!("/notifications/threads/{id}"), &c.token).send();
        }
    }
    Ok(interval)
}

fn schedule(seconds: u64) {
    yi_plugin::host::set_timer(seconds.max(30) * 1000, false);
}

struct GitHub;

impl Plugin for GitHub {
    fn start() -> Result<(), String> {
        let c = config()?;
        me(&c.token)?;
        yi_plugin::host::set_timer(1000, false);
        Ok(())
    }

    fn on_timer(_id: u32) {
        let want = config().ok().and_then(|c| c.poll_seconds).unwrap_or(60);
        match poll() {
            Ok(interval) => {
                yi_plugin::host::health("ok", "");
                schedule(want.max(interval));
            }
            Err(e) => {
                yi_plugin::host::health("error", &e);
                schedule(want.max(120));
            }
        }
    }

    fn reply(thread: &str, _reply_to: Option<&str>, key: &str, text: &str) -> Result<SendResult, String> {
        let (repo, n) = parse_thread(thread).ok_or_else(|| format!("Can't reply in this conversation: {thread}"))?;
        let c = config()?;
        yi_plugin::once(key, || {
            let r = api("POST", &format!("/repos/{repo}/issues/{n}/comments"), &c.token).json(json!({"body": text})).send()?;
            match r.status {
                200..=299 => Ok(SendResult::Sent),
                404 | 410 => Ok(SendResult::Gone),
                403 if r.body.contains("locked") => Ok(SendResult::Gone),
                s => Err(format!("HTTP {s}: {}", cut(&r.body, 300))),
            }
        })
    }

    fn read_context(thread: &str, cursor: Option<&str>) -> Result<ContextPage, String> {
        let (repo, n) = parse_thread(thread).ok_or_else(|| format!("No issue or pull request to act on: {thread}"))?;
        let c = config()?;
        let page: u32 = cursor.and_then(|c| c.parse().ok()).unwrap_or(1);
        let mut messages = vec![];
        let msg = |v: &Value| ContextMessage {
            id: v["id"].as_u64().map(|i| i.to_string()).unwrap_or_default(),
            sender: Sender { id: v["user"]["login"].as_str().unwrap_or("").into(), name: None, bot: v["user"]["type"] == "Bot" },
            at: 0,
            text: cut(v["body"].as_str().unwrap_or(""), MAX_TEXT),
            url: v["html_url"].as_str().map(str::to_string),
        };
        if page == 1 {
            let issue: Value = api("GET", &format!("/repos/{repo}/issues/{n}"), &c.token).send()?.expect_json()?;
            let mut first = msg(&issue);
            first.text = cut(&format!("{}\n\n{}", issue["title"].as_str().unwrap_or(""), issue["body"].as_str().unwrap_or("")), MAX_TEXT);
            messages.push(first);
        }
        let comments: Vec<Value> = api("GET", &format!("/repos/{repo}/issues/{n}/comments?per_page=30&page={page}"), &c.token).send()?.expect_json()?;
        let more = comments.len() == 30;
        messages.extend(comments.iter().map(msg));
        Ok(ContextPage { messages, next: more.then(|| (page + 1).to_string()) })
    }

    fn tools() -> Vec<ToolDef> {
        vec![yi_plugin::tool(
            "api_get",
            "GET a GitHub REST API path, e.g. /repos/owner/repo/pulls/1/files. Returns the JSON body (cut at 20000 characters).",
            json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
            true,
        )]
    }

    fn call_tool(name: &str, input: &str) -> Result<String, String> {
        if name != "api_get" {
            return Err(format!("unknown tool {name}"));
        }
        let v: Value = serde_json::from_str(input).map_err(|e| e.to_string())?;
        let path = v["path"].as_str().unwrap_or("");
        if !path.starts_with('/') {
            return Err("path must start with /".into());
        }
        let c = config()?;
        let r = api("GET", path, &c.token).send()?;
        Ok(format!("HTTP {}\n{}", r.status, cut(&r.body, 20000)))
    }
}

yi_plugin::export!(GitHub);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threads_and_numbers() {
        assert_eq!(parse_thread("github:a/b#12"), Some(("a/b".into(), 12)));
        assert_eq!(parse_thread("github:a/b:ci"), None);
        assert_eq!(parse_thread("feishu:x#1"), None);
        assert_eq!(number_of("https://api.github.com/repos/a/b/issues/7"), Some(7));
    }

    #[test]
    fn notifications_become_events() {
        let n = json!({
            "id": "1", "updated_at": "2026-01-01T00:00:00Z", "reason": "mention",
            "repository": {"full_name": "a/b"},
            "subject": {"type": "PullRequest", "title": "Fix it", "url": "https://api.github.com/repos/a/b/pulls/3"},
        });
        let c = json!({"user": {"login": "ann", "type": "User"}, "body": "@me please look", "html_url": "https://github.com/a/b/pull/3#c"});
        let e = to_event(&n, &c, "me").unwrap().0;
        assert_eq!(e.thread, "github:a/b#3");
        assert!(e.addressed);
        assert_eq!(e.sender.id, "ann");
        assert!(e.text.contains("PR #3") && e.text.contains("please look"));
        // Own comments are skipped, bots are marked
        assert!(to_event(&n, &json!({"user": {"login": "ME"}}), "me").is_none());
        let b = to_event(&n, &json!({"user": {"login": "dependabot[bot]", "type": "Bot"}}), "me").unwrap().0;
        assert!(b.sender.bot);
        // CI is ambient
        let ci = json!({"id": "2", "reason": "ci_activity", "repository": {"full_name": "a/b"}, "subject": {"type": "CheckSuite", "title": "CI failed"}});
        let e = to_event(&ci, &Value::Null, "me").unwrap().0;
        assert!(!e.addressed);
        assert_eq!(e.thread, "github:a/b:ci");
    }
}
