//! Start a temporary tool process to query the model list and account quota without creating a session (ACP tools need an empty session, created in a dedicated directory).

use crate::adapters::{self, JsonProc};
use crate::hub::Hub;
use crate::util::now_ms;
use anyhow::{anyhow, bail, Result};
use yi_proto::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

const TIMEOUT: Duration = Duration::from_secs(30);
/// How long quota results are cached, to avoid restarting processes on every page switch
const QUOTA_TTL_MS: i64 = 60_000;

static QUOTA_CACHE: Mutex<Option<HashMap<String, (i64, Value)>>> = Mutex::new(None);

enum Step {
    Send(Vec<Value>),
    Wait,
    Done,
}

fn probe_dir() -> PathBuf {
    let d = crate::paths::base_dir().join("probe");
    let _ = std::fs::create_dir_all(&d);
    d
}

#[derive(Clone, Default)]
struct ProbeEnv {
    env: BTreeMap<String, String>,
    env_unset: Vec<String>,
    auth_kind: Option<AccountAuthKind>,
    auth_method: Option<String>,
    identity: AccountIdentity,
    account_id: Option<String>,
    account_label: Option<String>,
    cursor_fallback_auth: Option<Vec<u8>>,
}

impl ProbeEnv {
    fn from_resolved(res: &crate::accounts::ResolvedAccount) -> Self {
        Self {
            env: res.env.clone(),
            env_unset: res.env_unset.clone(),
            auth_kind: Some(res.auth_kind),
            auth_method: res.auth_method.clone(),
            identity: res.identity.clone(),
            account_id: Some(res.id.clone()),
            account_label: Some(res.label.clone()),
            cursor_fallback_auth: None,
        }
    }
}

/// Start the process with profile env, send first, then pass each line received to on until Done or timeout
async fn converse(
    tool: ToolKind,
    settings: &DaemonSettings,
    args: &[&str],
    first: Vec<Value>,
    on: impl FnMut(&Value) -> Step,
) -> Result<()> {
    converse_in_env(tool, settings, &ProbeEnv::default(), args, &probe_dir(), TIMEOUT, first, on).await
}

async fn converse_env(
    tool: ToolKind,
    settings: &DaemonSettings,
    penv: &ProbeEnv,
    args: &[&str],
    first: Vec<Value>,
    on: impl FnMut(&Value) -> Step,
) -> Result<()> {
    converse_in_env(tool, settings, penv, args, &probe_dir(), TIMEOUT, first, on).await
}

async fn converse_in_env(
    tool: ToolKind,
    settings: &DaemonSettings,
    penv: &ProbeEnv,
    args: &[&str],
    cwd: &std::path::Path,
    timeout: Duration,
    first: Vec<Value>,
    mut on: impl FnMut(&Value) -> Step,
) -> Result<()> {
    let program = crate::tools::resolve(tool, settings)
        .ok_or_else(|| anyhow!("这台机器上没有找到 {}", tool.as_str()))?;
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let mut p = JsonProc::spawn_with_env(&program, &args, cwd, &penv.env, &penv.env_unset)?;
    for m in first {
        p.send(&m).await?;
    }
    let r = tokio::time::timeout(timeout, async {
        while let Ok(Some(line)) = p.stdout.next_line().await {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            match on(&v) {
                Step::Send(ms) => {
                    for m in ms {
                        p.send(&m).await?;
                    }
                }
                Step::Wait => {}
                Step::Done => return Ok(()),
            }
        }
        bail!("{} 意外退出", tool.as_str())
    })
    .await;
    let _ = p.finish(true).await;
    match r {
        Ok(r) => r,
        Err(_) => bail!("查询 {} 超时", tool.as_str()),
    }
}

/// Per-tool results
#[derive(Default)]
struct Probe {
    models: Vec<ModelOption>,
    quota: Option<Value>,
}

async fn claude(settings: &DaemonSettings, penv: &ProbeEnv, want_quota: bool) -> Result<Probe> {
    let mut out = Probe::default();
    let mut account = Value::Null;
    let init = json!({"type":"control_request","request_id":"init","request":{"subtype":"initialize"}});
    converse_env(
        ToolKind::Claude,
        settings,
        penv,
        &["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"],
        vec![init],
        |v| {
            if v["type"] != "control_response" {
                return Step::Wait;
            }
            let r = &v["response"]["response"];
            match v["response"]["request_id"].as_str() {
                Some("init") => {
                    out.models = adapters::claude::parse_models(r);
                    account = r["account"].clone();
                    if !want_quota {
                        return Step::Done;
                    }
                    Step::Send(vec![json!({"type":"control_request","request_id":"usage",
                        "request":{"subtype":"get_usage"}})])
                }
                Some("usage") => {
                    out.quota = Some(claude_quota(&account, r));
                    Step::Done
                }
                _ => Step::Wait,
            }
        },
    )
    .await?;
    Ok(out)
}

fn claude_quota(account: &Value, r: &Value) -> Value {
    let labels = [
        ("five_hour", "5 小时"),
        ("seven_day", "每周"),
        ("seven_day_opus", "每周 · Opus"),
        ("seven_day_sonnet", "每周 · Sonnet"),
    ];
    let rl = &r["rate_limits"];
    let windows: Vec<Value> = labels
        .iter()
        .filter(|(k, _)| rl[*k].is_object())
        .map(|(k, label)| {
            json!({"label": label, "usedPercent": rl[*k]["utilization"], "resetsAt": rl[*k]["resets_at"]})
        })
        .collect();
    let note = (!r["rate_limits_available"].as_bool().unwrap_or(false))
        .then_some("当前账号没有订阅额度信息（例如使用 API Key 计费）");
    json!({
        "account": {"email": account["email"], "plan": account["subscriptionType"]},
        "windows": windows,
        "note": note,
    })
}

const CODEX_LOGIN_EXPIRED: &str = "ChatGPT 登录已失效（令牌过期且无法刷新），请点击「重新登录」";

async fn codex(settings: &DaemonSettings, penv: &ProbeEnv, want_quota: bool) -> Result<Probe> {
    let mut out = Probe::default();
    let (mut account, mut limits) = (Value::Null, Value::Null);
    let mut requires_auth = false;
    let mut waiting = 0;
    let init = json!({"id":1,"method":"initialize",
        "params":{"clientInfo":{"name":crate::adapters::codex::CLIENT_NAME,"title":"Yi Console","version":DAEMON_VERSION}}});
    converse_env(ToolKind::Codex, settings, penv, &["app-server"], vec![init], |v| {
        let Some(id) = v["id"].as_u64().filter(|_| v.get("method").is_none()) else {
            return Step::Wait;
        };
        let r = &v["result"];
        match id {
            1 => {
                let mut m = vec![
                    json!({"method":"initialized"}),
                    json!({"id":4,"method":"model/list","params":{}}),
                ];
                waiting = 1;
                if want_quota {
                    m.push(json!({"id":2,"method":"account/read","params":{}}));
                    m.push(json!({"id":3,"method":"account/rateLimits/read"}));
                    waiting = 3;
                }
                return Step::Send(m);
            }
            2 => {
                account = r["account"].clone();
                requires_auth = r["requiresOpenaiAuth"].as_bool().unwrap_or(false);
            }
            3 => limits = r["rateLimits"].clone(),
            4 => out.models = adapters::codex::parse_models(r),
            _ => return Step::Wait,
        }
        waiting -= 1;
        if waiting == 0 {
            Step::Done
        } else {
            Step::Wait
        }
    })
    .await?;
    // Codex reports a ChatGPT login whose tokens can't be refreshed as "no account" (and rate limits fail with 401).
    // Say so instead of "no quota (API key billing)". Only for OAuth accounts: API key accounts have no rate limits either.
    if want_quota && penv.auth_kind == Some(AccountAuthKind::Oauth) && account.is_null() && requires_auth {
        bail!("{CODEX_LOGIN_EXPIRED}");
    }
    if want_quota {
        out.quota = Some(codex_quota(&account, &limits));
    }
    Ok(out)
}

fn window_label(mins: u64) -> String {
    match mins {
        300 => "5 小时".into(),
        10080 => "每周".into(),
        m if m % 1440 == 0 => format!("{} 天", m / 1440),
        m if m % 60 == 0 => format!("{} 小时", m / 60),
        m => format!("{m} 分钟"),
    }
}

fn codex_quota(account: &Value, rl: &Value) -> Value {
    let windows: Vec<Value> = ["primary", "secondary"]
        .iter()
        .filter(|k| rl[**k].is_object())
        .map(|k| {
            let w = &rl[*k];
            json!({
                "label": window_label(w["windowDurationMins"].as_u64().unwrap_or(0)),
                "usedPercent": w["usedPercent"],
                // seconds -> milliseconds
                "resetsAt": w["resetsAt"].as_i64().map(|s| s * 1000),
            })
        })
        .collect();
    let note = rl.is_null().then_some("当前账号没有额度信息（例如使用 API Key 计费）");
    json!({
        "account": {"email": account["email"], "plan": account["planType"].as_str().or(rl["planType"].as_str())},
        "windows": windows,
        "note": note,
    })
}

/// Cursor / Kiro / Qoder / CodeBuddy / Antigravity: create an empty session to get the model list; Kiro then runs /usage
async fn acp(tool: ToolKind, settings: &DaemonSettings, penv: &ProbeEnv, want_quota: bool) -> Result<Probe> {
    let mut out = Probe::default();
    let cwd = probe_dir().display().to_string();
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,
        "clientCapabilities":adapters::acp::client_capabilities(tool),
        "clientInfo":{"name":"yi-console","version":DAEMON_VERSION}}});
    let kiro_usage = want_quota && tool == ToolKind::Kiro;
    let mut sid = String::new();
    // Every session Qoder creates shows up in its history, so delete it after asking
    let delete = tool == ToolKind::Qoder;
    let explicit_auth = penv.auth_method.clone();
    converse_env(tool, settings, penv, &[adapters::acp::acp_arg(tool)], vec![init], |v| {
        let Some(id) = v["id"].as_u64().filter(|_| v.get("method").is_none()) else {
            return Step::Wait;
        };
        let r = &v["result"];
        match id {
            1 => {
                if let Some(m) = explicit_auth.as_deref() {
                    Step::Send(vec![json!({"jsonrpc":"2.0","id":99,"method":"authenticate","params":{"methodId":m}})])
                } else {
                    Step::Send(vec![json!({"jsonrpc":"2.0","id":2,"method":"session/new",
                        "params":{"cwd":cwd,"mcpServers":[]}})])
                }
            }
            99 => Step::Send(vec![json!({"jsonrpc":"2.0","id":2,"method":"session/new",
                "params":{"cwd":cwd,"mcpServers":[]}})]),
            2 => {
                out.models = if tool == ToolKind::Antigravity { adapters::acp::antigravity_models(r) } else { adapters::acp::parse_models(r) };
                sid = r["sessionId"].as_str().unwrap_or("").to_string();
                if delete && !sid.is_empty() {
                    return Step::Send(vec![json!({"jsonrpc":"2.0","id":4,"method":"session/delete",
                        "params":{"sessionId":sid}})]);
                }
                if !kiro_usage {
                    return Step::Done;
                }
                Step::Send(vec![json!({"jsonrpc":"2.0","id":3,"method":"_kiro.dev/commands/execute",
                    "params":{"sessionId":sid,"command":{"command":"usage","args":{}}}})])
            }
            3 => {
                out.quota = Some(kiro_quota(r));
                Step::Done
            }
            4 => Step::Done,
            _ => Step::Wait,
        }
    })
    .await?;
    Ok(out)
}

/// Cursor lists a model's parameters only after a session switches to it. Switch an empty session to `model`,
/// read them, and switch back, since Cursor remembers the last model for its next session. `cwd` is the
/// session's directory: Cursor refuses to set up a session in some directories ("Failed to initialize session services")
pub async fn cursor_model_config(
    hub: &Arc<Hub>,
    settings: &DaemonSettings,
    model: &str,
    cwd: &std::path::Path,
    account_id: Option<&str>,
) -> Result<Vec<ModelConfigOption>> {
    let resolved = hub.accounts.resolve(ToolKind::Cursor, account_id);
    if let Some(res) = &resolved {
        if res.auth_kind == AccountAuthKind::Oauth {
            hub.accounts.ensure_cursor_oauth_host_clean(&res.id)?;
        }
    }
    let penv = resolved.as_ref().map(ProbeEnv::from_resolved).unwrap_or_default();
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,
        "clientCapabilities":adapters::acp::client_capabilities(ToolKind::Cursor),
        "clientInfo":{"name":"yi-console","version":DAEMON_VERSION}}});
    let set = |id: u64, sid: &str, value: &str| json!({"jsonrpc":"2.0","id":id,"method":"session/set_config_option",
        "params":{"sessionId":sid,"configId":"model","value":value}});
    let (mut sid, mut original) = (String::new(), None::<String>);
    let mut found: Result<Vec<ModelConfigOption>> = Err(anyhow!("cursor 没有返回模型参数"));
    let cwd_s = cwd.display().to_string();
    converse_in_env(ToolKind::Cursor, settings, &penv, &["acp"], cwd, Duration::from_secs(90), vec![init], |v| {
        let Some(id) = v["id"].as_u64().filter(|_| v.get("method").is_none()) else { return Step::Wait };
        let r = &v["result"];
        let error = v["error"]["data"]["message"].as_str().or(v["error"]["message"].as_str()).map(str::to_string);
        match id {
            1 => Step::Send(vec![json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd_s,"mcpServers":[]}})]),
            2 => {
                if let Some(e) = error { found = Err(anyhow!("{e}")); return Step::Done; }
                sid = r["sessionId"].as_str().unwrap_or("").to_string();
                original = adapters::acp::config_params(r).and_then(|(m, _)| m);
                Step::Send(vec![set(3, &sid, model)])
            }
            // A failed switch is often Cursor's own model list request failing; try once more
            3 if error.is_some() => Step::Send(vec![set(4, &sid, model)]),
            3 | 4 => {
                if let Some(e) = error { found = Err(anyhow!("{e}")); return Step::Done; }
                found = adapters::acp::config_params(r).map(|(_, p)| p).ok_or_else(|| anyhow!("cursor 没有返回模型参数"));
                match original.as_deref().filter(|o| *o != model) {
                    Some(o) => Step::Send(vec![set(5, &sid, o)]),
                    None => Step::Done,
                }
            }
            5 => Step::Done,
            _ => Step::Wait,
        }
    })
    .await?;
    found
}

fn kiro_quota(r: &Value) -> Value {
    let d = &r["data"];
    let reset = d["billingCycleReset"].clone();
    let windows: Vec<Value> = d["usageBreakdowns"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|b| {
            json!({
                "label": b["displayName"].as_str().unwrap_or("用量"),
                "usedPercent": b["percentage"],
                "resetsAt": reset,
                "detail": format!("{} / {}",
                    fmt_num(b["used"].as_f64().unwrap_or(0.0)),
                    if b["hasLimit"].as_bool().unwrap_or(true) { fmt_num(b["limit"].as_f64().unwrap_or(0.0)) } else { "不限".into() }),
            })
        })
        .collect();
    let note = if r.is_null() || d.is_null() {
        r["message"].as_str().map(str::to_string)
    } else {
        None
    };
    json!({ "account": {"plan": d["planName"]}, "windows": windows, "note": note })
}

/// The signed-in email from `kiro-cli whoami --format json` (first line `{"accountType", "email", "region", "startUrl"}`,
/// followed by the profile as plain text); Kiro's login rows don't hold it
async fn kiro_whoami_email(settings: &DaemonSettings, penv: &ProbeEnv) -> Option<String> {
    let program = crate::tools::resolve(ToolKind::Kiro, settings)?;
    let args = ["whoami".to_string(), "--format".to_string(), "json".to_string()];
    let mut cmd: tokio::process::Command = adapters::std_command(&program, &args).into();
    for k in &penv.env_unset {
        cmd.env_remove(k);
    }
    for (k, v) in &penv.env {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let out = tokio::time::timeout(TIMEOUT, cmd.output()).await.ok()?.ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(text.lines().find(|l| l.trim_start().starts_with('{'))?).ok()?;
    v["email"].as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

fn fmt_num(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{x:.0}")
    } else {
        format!("{x:.2}")
    }
}

fn cursor_api_endpoint(penv: &ProbeEnv) -> String {
    penv.env
        .get("CURSOR_API_ENDPOINT")
        .or_else(|| penv.env.get("CURSOR_API_BASE_URL"))
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| crate::tools::host_env_var("CURSOR_API_ENDPOINT"))
        .or_else(|| crate::tools::host_env_var("CURSOR_API_BASE_URL"))
        .unwrap_or_else(|| "https://api2.cursor.sh".to_string())
        .trim_end_matches('/')
        .to_string()
}

fn cursor_auth_api_key(bytes: &[u8]) -> Option<String> {
    crate::accounts::cursor_auth_api_key(bytes)
}

/// Validate a `CURSOR_API_KEY` on save/capture:
/// - Rejects HTTP 401/403 immediately with a clear invalid-key error.
/// - Returns `Ok(Some((identity, raw_quota)))` when the Cursor API responds.
/// - Returns `Ok(None)` on offline / transport errors so offline setup still works.
pub fn verify_cursor_api_key(
    api_key: &str,
    base_url: Option<&str>,
) -> Result<Option<(AccountIdentity, Value)>> {
    let key = api_key.trim();
    if key.is_empty() {
        return Ok(None);
    }
    let endpoint = base_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| crate::tools::host_env_var("CURSOR_API_ENDPOINT"))
        .or_else(|| crate::tools::host_env_var("CURSOR_API_BASE_URL"))
        .unwrap_or_else(|| "https://api2.cursor.sh".to_string())
        .trim_end_matches('/')
        .to_string();
    let url = format!("{endpoint}/auth/exchange_user_api_key");
    let resp = ureq::post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {key}"))
        .config()
        .timeout_global(Some(Duration::from_secs(12)))
        .build()
        .send(&b"{}"[..]);
    let mut resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(401 | 403)) => bail!("CURSOR_API_KEY 无效或没有权限"),
        Err(ureq::Error::StatusCode(code)) => bail!("CURSOR_API_KEY 校验失败（HTTP {code}）"),
        Err(_) => return Ok(None),
    };
    let Ok(body) = resp.body_mut().read_to_string() else {
        return Ok(None);
    };
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return Ok(None);
    };
    let Some(token) = v["accessToken"].as_str().filter(|t| !t.is_empty()) else {
        bail!("CURSOR_API_KEY 交换访问令牌失败");
    };
    let Ok(q) = cursor_usage_with_token(&endpoint, token) else {
        return Ok(None);
    };
    let email = q["account"]["email"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let plan = q["account"]["plan"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let ident = AccountIdentity {
        account_key: email.clone(),
        email,
        plan,
        workspace: base_url
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from),
    };
    Ok(Some((ident, q)))
}

pub fn cache_verified_cursor_quota(base_dir: &Path, acc: &AccountProfileView, raw_quota: Value) {
    let now = now_ms();
    let penv = ProbeEnv {
        auth_kind: Some(acc.auth_kind),
        identity: acc.identity.clone(),
        account_id: Some(acc.id.clone()),
        account_label: Some(acc.label.clone()),
        ..Default::default()
    };
    let enriched = enrich_quota(raw_quota, ToolKind::Cursor, &penv, now);
    store_quota_cache(base_dir, ToolKind::Cursor, Some(&acc.id), &enriched, now);
}

fn cursor_token_from_bytes(bytes: &[u8]) -> Result<String> {
    let auth: Value = serde_json::from_slice(bytes)?;
    auth["accessToken"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(String::from)
        .ok_or_else(|| anyhow!("cursor-agent 还没有登录"))
}

fn cursor_exchange_api_key(endpoint: &str, api_key: &str) -> Result<String> {
    let url = format!("{endpoint}/auth/exchange_user_api_key");
    let mut resp = ureq::post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {api_key}"))
        .config()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .send(&b"{}"[..])
        .map_err(|e| match e {
            ureq::Error::StatusCode(401 | 403) => anyhow!("CURSOR_API_KEY 无效或没有权限"),
            e => anyhow!("{e}"),
        })?;
    let v: Value = serde_json::from_str(&resp.body_mut().read_to_string()?)?;
    v["accessToken"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(String::from)
        .ok_or_else(|| anyhow!("CURSOR_API_KEY 交换访问令牌失败"))
}

fn cursor_call_at(endpoint: &str, token: &str, method: &str) -> Result<Value> {
    let url = format!("{endpoint}/aiserver.v1.DashboardService/{method}");
    let mut resp = ureq::post(&url)
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .header("Authorization", &format!("Bearer {token}"))
        .config()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .send(&b"{}"[..])
        .map_err(|e| match e {
            ureq::Error::StatusCode(401 | 403) => anyhow!("登录已过期，运行一次 cursor-agent 刷新登录"),
            e => anyhow!("{e}"),
        })?;
    Ok(serde_json::from_str(&resp.body_mut().read_to_string()?)?)
}

fn cursor_usage_with_token(endpoint: &str, token: &str) -> Result<Value> {
    let usage = cursor_call_at(endpoint, token, "GetCurrentPeriodUsage")?;
    let plan = cursor_call_at(endpoint, token, "GetPlanInfo").unwrap_or(Value::Null);
    let hard = cursor_call_at(endpoint, token, "GetHardLimit").unwrap_or(Value::Null);
    let me = cursor_call_at(endpoint, token, "GetMe").unwrap_or(Value::Null);
    let mut q = cursor_quota(&usage, &plan, &hard);
    if let Some(email) = me["email"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        q["account"]["email"] = json!(email);
    }
    Ok(q)
}

async fn cursor_api_key_account(
    api_key: &str,
    endpoint: &str,
    identity: &AccountIdentity,
) -> Result<Value> {
    let key = api_key.to_string();
    let ep = endpoint.to_string();
    let mut q = tokio::task::spawn_blocking(move || {
        let token = cursor_exchange_api_key(&ep, &key)?;
        cursor_usage_with_token(&ep, &token)
    })
    .await??;
    if q["account"]["email"].is_null() {
        if let Some(email) = &identity.email {
            q["account"]["email"] = json!(email);
        }
    }
    if q["account"]["plan"].is_null() {
        if let Some(plan) = &identity.plan {
            q["account"]["plan"] = json!(plan);
        }
    }
    Ok(q)
}

/// Cursor: resolve host `CURSOR_API_KEY` precedence, or isolated OAuth account from `cursor-agent about` + DashboardService.
async fn cursor_account(settings: &DaemonSettings, penv: &ProbeEnv) -> Result<Value> {
    let endpoint = cursor_api_endpoint(penv);
    if penv.auth_kind.is_none() {
        if let Some(env_key) = crate::tools::host_env_var("CURSOR_API_KEY") {
            return cursor_api_key_account(&env_key, &endpoint, &penv.identity).await;
        }
        if let Ok(bytes) = std::fs::read(cursor_auth_file()) {
            if let Some(file_key) = cursor_auth_api_key(&bytes) {
                return cursor_api_key_account(&file_key, &endpoint, &penv.identity).await;
            }
        }
    }

    // If host auth.json was overwritten by a CURSOR_API_KEY run, probe the OAuth snapshot directly instead of touching disk.
    if penv.auth_kind == Some(AccountAuthKind::Oauth) {
        if let Ok(bytes) = std::fs::read(cursor_auth_file()) {
            if crate::accounts::cursor_bytes_are_api_key(&bytes) {
                if let Some(clean_bytes) = &penv.cursor_fallback_auth {
                    let token = cursor_token_from_bytes(clean_bytes)?;
                    let ep = endpoint.clone();
                    let mut q = tokio::task::spawn_blocking(move || cursor_usage_with_token(&ep, &token)).await??;
                    if q["account"]["email"].is_null() {
                        if let Some(email) = &penv.identity.email {
                            q["account"]["email"] = json!(email);
                        }
                    }
                    return Ok(q);
                }
                bail!("当前 Cursor 授权账号保存的凭据实际为 API Key 而非网页授权登录，请在账号管理中点击「重新登录」完成授权");
            }
        }
    }

    let program = crate::tools::resolve(ToolKind::Cursor, settings)
        .ok_or_else(|| anyhow!("这台机器上没有找到 cursor"))?;
    let mut cmd: tokio::process::Command =
        adapters::std_command(&program, &["about".to_string()]).into();
    for k in &penv.env_unset {
        cmd.env_remove(k);
    }
    for (k, v) in &penv.env {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let out = tokio::time::timeout(TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow!("查询 cursor 超时"))??;
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(name).map(|v| v.trim().to_string()))
            .filter(|v| !v.is_empty())
    };
    let email = field("User Email");
    let tier = field("Subscription Tier");
    // Same as /usage in cursor-agent: query the backend with the token it saved at login
    let ep = endpoint.clone();
    match tokio::task::spawn_blocking(move || cursor_usage(&ep)).await? {
        Ok(mut q) => {
            if q["account"]["plan"].is_null() {
                q["account"]["plan"] = json!(tier);
            }
            if q["account"]["email"].is_null() || email.is_some() {
                if let Some(e) = email {
                    q["account"]["email"] = json!(e);
                }
            }
            Ok(q)
        }
        Err(e) => Ok(json!({
            "account": {"email": email, "plan": tier},
            "windows": [],
            "note": format!("读取用量失败：{e}。也可以在 cursor-agent 里输入 /usage，或到 cursor.com/dashboard 查看"),
        })),
    }
}

/// Where cursor-agent keeps its login info (matching its own implementation)
fn cursor_auth_file() -> std::path::PathBuf {
    let home = crate::paths::home();
    if cfg!(windows) {
        std::env::var("APPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| home.join("AppData").join("Roaming"))
            .join("Cursor")
            .join("auth.json")
    } else if cfg!(target_os = "macos") {
        home.join(".cursor").join("auth.json")
    } else {
        std::env::var("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| home.join(".config"))
            .join("cursor")
            .join("auth.json")
    }
}

fn cursor_usage(endpoint: &str) -> Result<Value> {
    let path = cursor_auth_file();
    let bytes = std::fs::read(&path).map_err(|_| anyhow!("没有找到 cursor-agent 的登录信息"))?;
    let token = cursor_token_from_bytes(&bytes)?;
    cursor_usage_with_token(endpoint, &token)
}

/// Amounts are in cents, times are millisecond strings
fn cursor_quota(usage: &Value, plan: &Value, hard: &Value) -> Value {
    let num = |v: &Value| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()));
    let reset = num(&usage["billingCycleEnd"])
        .filter(|v| *v > 0.0)
        .or_else(|| num(&plan["planInfo"]["billingCycleEnd"]))
        .map(|v| json!(v as i64));
    let pu = &usage["planUsage"];
    let mut windows = vec![];
    // Same as cursor-agent's /usage: three percentages for total plan usage, Auto, and named models (API)
    if pu.is_object() {
        let used = num(&pu["includedSpend"]).unwrap_or(0.0);
        let limit = num(&pu["limit"]).unwrap_or(0.0);
        let pct = num(&pu["totalPercentUsed"]).or((limit > 0.0).then(|| used / limit * 100.0));
        windows.push(json!({"label": "套餐内用量", "usedPercent": pct, "resetsAt": reset}));
        for (k, label) in [("autoPercentUsed", "Auto 模型"), ("apiPercentUsed", "指定模型（API）")] {
            windows.push(json!({"label": label, "usedPercent": num(&pu[k]).unwrap_or(0.0), "resetsAt": reset}));
        }
    }
    // Pay-as-you-go, using cursor-agent's rules (fields with 0 values may be omitted in proto3 JSON)
    let sl = &usage["spendLimitUsage"];
    let used = num(&sl["individualUsed"]).unwrap_or(0.0) / 100.0;
    let own = num(&sl["individualLimit"]).map(|c| if c > 0.0 { Some(c / 100.0) } else { None });
    let hard_obj = hard.is_object() && !hard.as_object().unwrap().is_empty();
    let no_ubp = hard["noUsageBasedAllowed"].as_bool() == Some(true);
    let hard_limit = num(&hard["hardLimit"]).unwrap_or(0.0);
    enum Od {
        Fixed(f64),
        Unlimited,
        Disabled,
        Unknown,
    }
    let od = if sl["limitType"] == "team" {
        match own {
            Some(Some(l)) => Od::Fixed(l),
            Some(None) => Od::Disabled,
            None if hard_obj => {
                if no_ubp || hard_limit <= 0.0 {
                    Od::Disabled
                } else {
                    Od::Unlimited
                }
            }
            None if num(&sl["pooledLimit"]).unwrap_or(0.0) > 0.0 => Od::Unlimited,
            None => Od::Unknown,
        }
    } else if !hard_obj {
        match own {
            Some(Some(l)) => Od::Fixed(l),
            Some(None) => Od::Disabled,
            None => Od::Unknown,
        }
    } else if no_ubp {
        Od::Disabled
    } else if hard_limit >= 2147483647.0 {
        Od::Unlimited
    } else if hard_limit > 0.0 {
        Od::Fixed(hard_limit)
    } else {
        Od::Disabled
    };
    let money = |d: f64| format!("${d:.2}");
    let (pct, detail) = match od {
        Od::Fixed(l) => (Some(used / l * 100.0), format!("{} / {}", money(used), money(l))),
        Od::Unlimited => (None, format!("已用 {}，不限额", money(used))),
        Od::Disabled if used > 0.0 => (None, format!("已用 {}，已关闭", money(used))),
        Od::Disabled => (None, "未开启".to_string()),
        Od::Unknown => (None, if used > 0.0 { format!("已用 {}", money(used)) } else { "查不到".to_string() }),
    };
    if sl.is_object() || hard_obj {
        windows.push(json!({"label": "按量付费", "usedPercent": pct, "resetsAt": reset, "detail": detail}));
    }
    let note = if pu.is_object() {
        Value::Null
    } else {
        json!("这个套餐在命令行里查不到用量明细，请到 cursor.com/dashboard 查看")
    };
    json!({ "account": {"plan": plan["planInfo"]["planName"]}, "windows": windows, "note": note })
}

/// Qoder: send get_usage_info over the stream-json control protocol to get the credit quota (--no-session-persistence avoids leaving an empty session)
async fn qoder_account(settings: &DaemonSettings, penv: &ProbeEnv) -> Result<Value> {
    let mut account = Value::Null;
    let mut quota = Value::Null;
    let init = json!({"type":"control_request","request_id":"init","request":{"subtype":"initialize"}});
    converse_env(
        ToolKind::Qoder,
        settings,
        penv,
        &["-p", "--no-session-persistence", "--input-format", "stream-json", "--output-format", "stream-json"],
        vec![init],
        |v| {
            if v["type"] != "control_response" {
                return Step::Wait;
            }
            let r = &v["response"]["response"];
            match v["response"]["request_id"].as_str() {
                Some("init") => {
                    account = r["account"].clone();
                    Step::Send(vec![json!({"type":"control_request","request_id":"usage",
                        "request":{"subtype":"get_usage_info"}})])
                }
                Some("usage") => {
                    quota = qoder_quota(&account, r);
                    Step::Done
                }
                _ => Step::Wait,
            }
        },
    )
    .await?;
    Ok(quota)
}

fn qoder_quota(account: &Value, r: &Value) -> Value {
    let u = &r["usage"];
    let reset = u["expiresAt"].as_i64().filter(|v| *v > 0).map(|v| json!(v)).unwrap_or(Value::Null);
    let mut windows = vec![];
    let push_pkg = |windows: &mut Vec<Value>, label: &str, q: &Value, total_key: &str, resets_at: &Value| {
        let total = q[total_key].as_f64().unwrap_or(0.0);
        let used = q["used"].as_f64().unwrap_or(0.0);
        if total <= 0.0 && used <= 0.0 {
            return;
        }
        let pct = q["percentage"].as_f64().or((total > 0.0).then(|| used / total * 100.0));
        windows.push(json!({
            "label": label,
            "usedPercent": pct,
            "resetsAt": resets_at,
            "detail": format!("{} / {}", fmt_num(used), fmt_num(total)),
        }));
    };
    if u["userQuota"].is_object() {
        push_pkg(&mut windows, "套餐积分", &u["userQuota"], "total", &reset);
    }
    if u["addOnQuota"].is_object() {
        push_pkg(&mut windows, "增购积分", &u["addOnQuota"], "total", &Value::Null);
    }
    if u["orgResourcePackage"].is_object() {
        push_pkg(&mut windows, "团队资源包", &u["orgResourcePackage"], "cap", &reset);
    }
    for p in u["dedicatedResourcePackages"].as_array().into_iter().flatten() {
        if p["available"].as_bool() == Some(false) {
            continue;
        }
        let label = p["displayLabels"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|l| l["dimension"] == "title")
            .and_then(|l| l["valueI18n"]["zh-CN"].as_str().or(l["value"].as_str()))
            .or(p["name"].as_str())
            .unwrap_or("专属资源包");
        let pkg_reset = p["expiresAt"].as_i64().filter(|v| *v > 0).map(|v| json!(v)).unwrap_or(Value::Null);
        push_pkg(&mut windows, label, p, "total", &pkg_reset);
    }
    let note = if windows.is_empty() {
        r["usage_error"]
            .as_str()
            .map(str::to_string)
            .or_else(|| Some("当前账号没有返回额度明细，请到 qoder.com/account/usage 查看".to_string()))
    } else {
        None
    };
    json!({
        "account": {
            "email": account["email"].as_str().or(account["name"].as_str()),
            "plan": account["subscriptionType"],
        },
        "windows": windows,
        "note": note,
    })
}

/// CodeBuddy: get the login token from ACP's `_codebuddy.ai/getUserInfo`, then query the backend for credit and resource pack totals
async fn codebuddy_account(settings: &DaemonSettings) -> Result<Value> {
    let mut user = Value::Null;
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,
        "clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},
        "clientInfo":{"name":"yi-console","version":DAEMON_VERSION}}});
    converse(ToolKind::Codebuddy, settings, &["--acp"], vec![init], |v| {
        let Some(id) = v["id"].as_u64().filter(|_| v.get("method").is_none()) else {
            return Step::Wait;
        };
        let r = &v["result"];
        match id {
            1 => Step::Send(vec![json!({"jsonrpc":"2.0","id":2,"method":"_codebuddy.ai/getUserInfo","params":{}})]),
            2 => {
                user = r["userInfo"].clone();
                Step::Done
            }
            _ => Step::Wait,
        }
    })
    .await?;
    let email = user["userNickname"]
        .as_str()
        .or(user["userName"].as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let ent_name = user["enterpriseName"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    match tokio::task::spawn_blocking(move || codebuddy_usage(&user)).await? {
        Ok(q) => Ok(q),
        Err(e) => Ok(json!({
            "account": {"email": email, "plan": ent_name},
            "windows": [],
            "note": format!("读取用量失败：{e}"),
        })),
    }
}

/// Antigravity: `agy -p /usage` and `/credits` answer without starting a turn, and `--output-format json`
/// carries the data under `command.data` (the slash command never reaches the model)
async fn antigravity_account(settings: &DaemonSettings, penv: &ProbeEnv) -> Result<Value> {
    let ask = |cmd: &'static str| async move {
        let mut data = Value::Null;
        converse_env(ToolKind::Antigravity, settings, penv, &["-p", cmd, "--output-format", "json"], vec![], |v| {
            if v.get("status").is_none() {
                return Step::Wait;
            }
            data = v.clone();
            Step::Done
        })
        .await?;
        if data["status"] != "SUCCESS" {
            bail!("{}", data["error"].as_str().filter(|e| !e.is_empty()).unwrap_or("agy 没有返回额度"));
        }
        anyhow::Ok(data["command"]["data"].clone())
    };
    let (usage, credits) = tokio::join!(ask("/usage"), ask("/credits"));
    let mut q = antigravity_quota(&usage?, credits.as_ref().unwrap_or(&Value::Null));
    if let Some(email) = &penv.identity.email {
        q["account"]["email"] = json!(email);
    }
    if let Some(plan) = &penv.identity.plan {
        q["account"]["plan"] = json!(plan);
    }
    Ok(q)
}

/// `/usage` data: groups of models sharing a 5-hour and a weekly bucket, each with remaining_fraction and reset_time
fn antigravity_quota(usage: &Value, credits: &Value) -> Value {
    let mut windows = vec![];
    for g in usage["groups"].as_array().into_iter().flatten() {
        // "Gemini Models" -> "Gemini", "Claude and GPT models" -> "Claude / GPT": the cards are narrow
        let group = g["name"].as_str().unwrap_or("").trim_end_matches(" Models").trim_end_matches(" models").replace(" and ", " / ");
        for b in g["buckets"].as_array().into_iter().flatten() {
            let window = match b["window"].as_str() {
                Some("5h") => "5 小时".to_string(),
                Some("weekly") => "每周".to_string(),
                _ => b["name"].as_str().unwrap_or("").to_string(),
            };
            let used = b["remaining_fraction"].as_f64().map(|r| ((1.0 - r) * 1000.0).round() / 10.0);
            windows.push(json!({
                "label": if group.is_empty() { window } else { format!("{group} {window}") },
                "usedPercent": used,
                "resetsAt": b["reset_time"],
            }));
        }
    }
    let note = match credits["remaining_credits"].as_f64() {
        Some(c) if c > 0.0 => Some(format!("AI 积分剩余 {}", fmt_num(c))),
        _ => None,
    };
    json!({ "account": {}, "windows": windows, "note": note })
}

fn codebuddy_call(base: &str, path: &str, token: &str, ent_id: Option<&str>) -> Result<Value> {
    let url = format!("{base}{path}");
    let mut req = ureq::post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        // copilot.tencent.com answers `User-Agent: ureq/…` with HTTP 403 / code 10085 (请求不合法) and no quota body.
        .header("User-Agent", "codebuddy-code")
        .header("Authorization", &format!("Bearer {token}"));
    if let Some(eid) = ent_id.filter(|s| !s.is_empty()) {
        req = req.header("X-Enterprise-Id", eid);
    }
    // Read the JSON error body. A 403 from this gateway is not always an expired login.
    let mut resp = req
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .send(&b"{}"[..])?;
    let status = resp.status().as_u16();
    let v: Value = serde_json::from_str(&resp.body_mut().read_to_string()?)
        .map_err(|_| anyhow!("后台返回的不是额度数据"))?;
    if status == 401 || (status == 403 && v["msg"].as_str().unwrap_or("").is_empty()) {
        bail!("登录已过期，请运行一次 codebuddy 重新登录");
    }
    if !(200..300).contains(&status) || v["data"].is_null() {
        bail!("{}", v["msg"].as_str().unwrap_or("后台没有返回额度数据"));
    }
    Ok(v)
}

fn codebuddy_usage(user: &Value) -> Result<Value> {
    let token = user["accessToken"]
        .as_str()
        .or(user["token"].as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow!("codebuddy 还没有登录"))?;
    let base = if user["authType"].as_str() == Some("external") {
        "https://www.codebuddy.ai"
    } else {
        "https://copilot.tencent.com"
    };
    let ent_id = user["enterpriseId"].as_str().filter(|s| !s.is_empty());
    let ent = ent_id
        .and_then(|eid| codebuddy_call(base, "/billing/meter/get-enterprise-user-usage", token, Some(eid)).ok())
        .unwrap_or(Value::Null);
    let summary = codebuddy_call(base, "/billing/meter/get-user-resource-summary", token, ent_id)?;
    Ok(codebuddy_quota(user, &summary, &ent))
}

fn codebuddy_quota(user: &Value, summary: &Value, ent: &Value) -> Value {
    let num = |v: &Value| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()));
    let mut windows = vec![];
    let ed = &ent["data"];
    if ed.is_object() {
        let used = num(&ed["credit"]).unwrap_or(0.0);
        let limit = num(&ed["limitNum"]).unwrap_or(0.0);
        let reset = ed["cycleEndTime"].as_str().or(ed["cycleResetTime"].as_str());
        if limit < 0.0 {
            windows.push(json!({"label": "企业共享额度", "usedPercent": Value::Null, "resetsAt": reset,
                "detail": format!("已用 {}，不限额", fmt_num(used))}));
        } else if limit > 0.0 || used > 0.0 {
            let pct = (limit > 0.0).then(|| used / limit * 100.0);
            windows.push(json!({"label": "企业共享额度", "usedPercent": pct, "resetsAt": reset,
                "detail": format!("{} / {}", fmt_num(used), fmt_num(limit))}));
        }
        let gift = &ed["exclusiveGift"];
        if gift["hasGift"].as_bool() == Some(true) {
            let g_used = num(&gift["usedCredits"]).unwrap_or(0.0);
            let g_total = num(&gift["totalCredits"]).unwrap_or(0.0);
            if g_total > 0.0 || g_used > 0.0 {
                let pct = (g_total > 0.0).then(|| g_used / g_total * 100.0);
                windows.push(json!({"label": "个人专属积分", "usedPercent": pct, "resetsAt": gift["nextResetAt"],
                    "detail": format!("{} / {}", fmt_num(g_used), fmt_num(g_total))}));
            }
        }
    }
    let sd = &summary["data"];
    for g in sd["CreditGroups"].as_array().into_iter().flatten() {
        let total = num(&g["CycleTotalCapacity"]).unwrap_or(0.0);
        let used = num(&g["CycleUsedCapacity"]).unwrap_or(0.0);
        if total <= 0.0 && used <= 0.0 {
            continue;
        }
        let pct = (total > 0.0).then(|| used / total * 100.0);
        let label = g["Name"].as_str().unwrap_or("积分");
        windows.push(json!({
            "label": label,
            "usedPercent": pct,
            "resetsAt": Value::Null,
            "detail": format!("{} / {}", fmt_num(used), fmt_num(total)),
        }));
    }
    let email = user["userNickname"]
        .as_str()
        .or(user["userName"].as_str())
        .filter(|s| !s.is_empty());
    let plan = sd["SubscriptionPackageName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| user["enterpriseName"].as_str().filter(|s| !s.is_empty()));
    let note = windows.is_empty().then_some("当前账号没有可用积分包");
    json!({
        "account": {"email": email, "plan": plan},
        "windows": windows,
        "note": note,
    })
}

async fn run_with_env(
    tool: ToolKind,
    settings: &DaemonSettings,
    penv: &ProbeEnv,
    want_quota: bool,
) -> Result<Probe> {
    if want_quota && penv.auth_kind == Some(AccountAuthKind::ApiKey) {
        if tool == ToolKind::Cursor {
            if let Some(api_key) = penv.env.get("CURSOR_API_KEY").map(String::as_str).map(str::trim).filter(|s| !s.is_empty()) {
                let endpoint = cursor_api_endpoint(penv);
                return Ok(Probe {
                    models: vec![],
                    quota: Some(cursor_api_key_account(api_key, &endpoint, &penv.identity).await?),
                });
            }
        }
        return Ok(Probe {
            models: vec![],
            quota: Some(api_key_quota(tool, penv)),
        });
    }
    match tool {
        ToolKind::Claude => claude(settings, penv, want_quota).await,
        ToolKind::Codex => codex(settings, penv, want_quota).await,
        ToolKind::Kiro => {
            let mut p = acp(tool, settings, penv, want_quota).await?;
            if let Some(q) = p.quota.as_mut().filter(|q| q["account"]["email"].is_null()) {
                if let Some(email) = kiro_whoami_email(settings, penv).await {
                    q["account"]["email"] = json!(email);
                }
            }
            Ok(p)
        }
        ToolKind::Qoder if want_quota => Ok(Probe {
            models: vec![],
            quota: Some(qoder_account(settings, penv).await?),
        }),
        ToolKind::Codebuddy if want_quota => Ok(Probe {
            models: vec![],
            quota: Some(codebuddy_account(settings).await?),
        }),
        ToolKind::Qoder | ToolKind::Codebuddy => acp(tool, settings, penv, false).await,
        ToolKind::Cursor if want_quota => Ok(Probe {
            models: vec![],
            quota: Some(cursor_account(settings, penv).await?),
        }),
        ToolKind::Cursor => acp(tool, settings, penv, false).await,
        ToolKind::Antigravity if want_quota => Ok(Probe {
            models: vec![],
            quota: Some(antigravity_account(settings, penv).await?),
        }),
        ToolKind::Antigravity => acp(tool, settings, penv, false).await,
        ToolKind::Shell => bail!("终端没有模型和额度"),
        ToolKind::Files => bail!("文件会话没有模型和额度"),
    }
}

fn api_key_quota(tool: ToolKind, penv: &ProbeEnv) -> Value {
    json!({
        "tool": tool.as_str(),
        "accountId": penv.account_id,
        "accountLabel": penv.account_label,
        "authKind": "apiKey",
        "isolation": "isolated",
        "account": {
            "email": penv.identity.email,
            "plan": penv.identity.plan.clone().or_else(|| penv.identity.workspace.clone()).or_else(|| Some("API Key".into())),
        },
        "windows": [],
        "note": "API Key 按量计费（无固定订阅周期额度限制）",
    })
}

fn quota_cache_key(tool: ToolKind, account_id: Option<&str>) -> String {
    match account_id {
        Some(id) => format!("{}:{id}", tool.as_str()),
        None => tool.as_str().to_string(),
    }
}

fn quota_file_path(base_dir: &Path, tool: ToolKind, account_id: &str) -> PathBuf {
    base_dir
        .join("accounts")
        .join(tool.as_str())
        .join(account_id)
        .join("quota.json")
}

/// Read the latest cached quota for an account (in-memory first, then persistent snapshot).
pub fn cached_quota(tool: ToolKind, account_id: &str, base_dir: &Path) -> Option<Value> {
    let key = quota_cache_key(tool, Some(account_id));
    if let Some((_, v)) = QUOTA_CACHE
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.get(&key))
    {
        return Some(v.clone());
    }
    let raw = std::fs::read_to_string(quota_file_path(base_dir, tool, account_id)).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let at = v["fetchedAt"].as_i64().unwrap_or(0);
    QUOTA_CACHE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(key, (at, v.clone()));
    Some(v)
}

pub fn clear_account_cache(tool: ToolKind, account_id: &str) {
    let key = quota_cache_key(tool, Some(account_id));
    if let Some(m) = QUOTA_CACHE.lock().unwrap().as_mut() {
        m.remove(&key);
    }
}

fn store_quota_cache(base_dir: &Path, tool: ToolKind, account_id: Option<&str>, v: &Value, at: i64) {
    let key = quota_cache_key(tool, account_id);
    QUOTA_CACHE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(key, (at, v.clone()));
    if let Some(aid) = account_id {
        let p = quota_file_path(base_dir, tool, aid);
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(s) = serde_json::to_string_pretty(v) {
            let _ = std::fs::write(&p, s);
            crate::util::restrict_file(&p);
        }
    }
}

fn enrich_quota(mut v: Value, tool: ToolKind, penv: &ProbeEnv, at: i64) -> Value {
    if v["account"].is_null() {
        v["account"] = json!({});
    }
    if v["account"]["email"].is_null() {
        if let Some(e) = &penv.identity.email {
            v["account"]["email"] = json!(e);
        }
    }
    if v["account"]["plan"].is_null() {
        if let Some(p) = &penv.identity.plan {
            v["account"]["plan"] = json!(p);
        }
    }
    v["tool"] = json!(tool.as_str());
    v["fetchedAt"] = json!(at);
    if let Some(aid) = &penv.account_id {
        v["accountId"] = json!(aid);
    }
    if let Some(lbl) = &penv.account_label {
        v["accountLabel"] = json!(lbl);
    }
    if let Some(ak) = penv.auth_kind {
        v["authKind"] = json!(ak);
    }
    v
}

pub async fn models(hub: &Arc<Hub>, tool: ToolKind, settings: &DaemonSettings) -> Result<Vec<ModelOption>> {
    let resolved = hub.accounts.resolve(tool, None);
    if tool == ToolKind::Cursor {
        if let Some(res) = &resolved {
            if res.auth_kind == AccountAuthKind::Oauth {
                hub.accounts.ensure_cursor_oauth_host_clean(&res.id)?;
            }
        }
    }
    let penv = resolved.as_ref().map(ProbeEnv::from_resolved).unwrap_or_default();
    Ok(run_with_env(tool, settings, &penv, false).await?.models)
}

/// Account quota: {tool, accountId?, accountLabel?, account:{email,plan}, windows:[{label,usedPercent,resetsAt,detail?}], note?, fetchedAt}
pub async fn quota(
    hub: &Arc<Hub>,
    tool: ToolKind,
    settings: &DaemonSettings,
    account_id: Option<&str>,
    refresh: bool,
) -> Result<Value> {
    let resolved = if let Some(aid) = account_id {
        Some(
            hub.accounts
                .resolve_exact(tool, aid)
                .ok_or_else(|| anyhow!("账号 {aid} 不存在"))?,
        )
    } else {
        hub.accounts.resolve(tool, None)
    };
    let effective_id = resolved.as_ref().map(|r| r.id.clone());
    let key = quota_cache_key(tool, effective_id.as_deref());

    if !refresh {
        let cache = QUOTA_CACHE.lock().unwrap();
        if let Some((at, v)) = cache.as_ref().and_then(|m| m.get(&key)) {
            if now_ms() - at < QUOTA_TTL_MS {
                return Ok(v.clone());
            }
        }
    }

    let mut penv = resolved
        .as_ref()
        .map(ProbeEnv::from_resolved)
        .unwrap_or_default();
    if tool == ToolKind::Cursor && penv.auth_kind == Some(AccountAuthKind::Oauth) {
        if let Some(aid) = &penv.account_id {
            if !hub.accounts.has_clean_cursor_oauth_snapshot(aid) {
                bail!("当前 Cursor 授权账号保存的凭据实际为 API Key 而非网页授权登录，请在账号管理中点击「重新登录」完成授权");
            }
            penv.cursor_fallback_auth = hub.accounts.load_cursor_oauth_auth_bytes(aid);
            if hub.accounts.active_account_id(ToolKind::Cursor).as_deref() == Some(aid.as_str())
                && !hub.has_running_host_session(ToolKind::Cursor)
            {
                let _ = hub.accounts.ensure_cursor_oauth_host_clean(aid);
            }
        }
    }

    // Prevent touching host credentials passively if probing an inactive HostActive OAuth account without refresh.
    if let Some(res) = &resolved {
        let active_id = hub.accounts.active_account_id(tool);
        if res.isolation == AccountIsolation::HostActive
            && active_id.as_deref() != Some(res.id.as_str())
        {
            if let Some(mut cached) = cached_quota(tool, &res.id, hub.accounts.base_dir()) {
                cached["stale"] = json!(true);
                return Ok(cached);
            }
            return Ok(json!({
                "tool": tool.as_str(),
                "accountId": res.id,
                "accountLabel": res.label,
                "authKind": res.auth_kind,
                "isolation": res.isolation,
                "account": { "email": res.identity.email, "plan": res.identity.plan },
                "windows": [],
                "needsActivationToProbe": true,
                "note": "系统级单活账号：切换为当前账号后自动探测，或点击「探测全部」",
                "fetchedAt": Value::Null,
            }));
        }
    }

    let p = match run_with_env(tool, settings, &penv, true).await {
        Ok(p) => p,
        Err(e) => {
            if let Some(aid) = effective_id.as_deref().filter(|_| e.to_string() == CODEX_LOGIN_EXPIRED) {
                hub.accounts.set_runtime_state(tool, aid, AccountState::NeedsLogin, Some(CODEX_LOGIN_EXPIRED.into()));
            }
            return Err(e);
        }
    };
    if !p.models.is_empty() {
        hub.remember_models(tool, &p.models);
    }
    let raw_v = p
        .quota
        .ok_or_else(|| anyhow!("{} 没有返回额度信息", tool.as_str()))?;
    let now = now_ms();
    let v = enrich_quota(raw_v, tool, &penv, now);
    store_quota_cache(hub.accounts.base_dir(), tool, effective_id.as_deref(), &v, now);
    if let Some(aid) = &effective_id {
        if v["error"].is_null() {
            hub.accounts.set_runtime_state(tool, aid, AccountState::Ready, None);
            hub.accounts.update_identity_if_missing(
                tool,
                aid,
                v["account"]["email"].as_str(),
                v["account"]["plan"].as_str(),
            );
        }
    }
    Ok(v)
}

/// Probe/collect quotas across all configured accounts (and native host defaults where unconfigured).
pub async fn quotas(
    hub: &Arc<Hub>,
    tool_filter: Option<ToolKind>,
    refresh: bool,
    refresh_inactive: bool,
) -> Result<Value> {
    let settings = hub.settings.read().unwrap().clone();
    let tools_list: Vec<ToolKind> = match tool_filter {
        Some(t) if t.supports_accounts() => vec![t],
        Some(_) => vec![],
        None => ToolKind::ALL
            .into_iter()
            .filter(|t| t.supports_accounts())
            .filter(|t| {
                crate::tools::resolve(*t, &settings).is_some()
                    || !hub.accounts.all_accounts_for_tool(*t).is_empty()
            })
            .collect(),
    };

    let mut out_tools = Vec::new();
    for tool in tools_list {
        let cap = crate::accounts::AccountManager::capability(tool);
        let accounts = hub.accounts.all_accounts_for_tool(tool);
        let active_id = hub.accounts.active_account_id(tool);

        if accounts.is_empty() {
            let native_res = if crate::tools::resolve(tool, &settings).is_some() {
                Some(quota(hub, tool, &settings, None, refresh).await)
            } else {
                None
            };
            let (native_quota, error) = match native_res {
                Some(Ok(q)) => (Some(q), None),
                Some(Err(e)) => (None, Some(e.to_string())),
                None => (None, None),
            };
            out_tools.push(json!({
                "tool": tool.as_str(),
                "oauthIsolation": cap.oauth_isolation,
                "activeAccountId": Value::Null,
                "accounts": [],
                "nativeQuota": native_quota,
                "error": error,
            }));
            continue;
        }

        // Split accounts into concurrently probeable (Isolated / active HostActive / ApiKey)
        // vs inactive HostActive OAuth accounts.
        let mut results: BTreeMap<String, Value> = BTreeMap::new();
        let sem = Arc::new(Semaphore::new(4));
        let mut isolated_tasks = tokio::task::JoinSet::new();
        let mut inactive_host_ids = Vec::new();

        for acc in &accounts {
            let iso = crate::accounts::AccountManager::effective_isolation(tool, acc.auth_kind);
            let is_active = active_id.as_deref() == Some(acc.id.as_str())
                || (active_id.is_none() && accounts.first().map(|a| &a.id) == Some(&acc.id));

            if iso == AccountIsolation::HostActive && !is_active {
                if !refresh_inactive {
                    let cached = cached_quota(tool, &acc.id, hub.accounts.base_dir()).map(|mut q| {
                        let age = now_ms() - q["fetchedAt"].as_i64().unwrap_or(0);
                        if age >= QUOTA_TTL_MS {
                            q["stale"] = json!(true);
                        }
                        q
                    });
                    let q = cached.unwrap_or_else(|| {
                        json!({
                            "tool": tool.as_str(),
                            "accountId": acc.id,
                            "accountLabel": acc.label,
                            "authKind": acc.auth_kind,
                            "isolation": iso,
                            "account": { "email": acc.identity.email, "plan": acc.identity.plan },
                            "windows": [],
                            "needsActivationToProbe": true,
                            "note": "系统级单活账号：切换为当前账号后自动探测，或点击「切换探测全部」",
                            "fetchedAt": Value::Null,
                        })
                    });
                    results.insert(acc.id.clone(), json!({
                        "accountId": acc.id,
                        "label": acc.label,
                        "authKind": acc.auth_kind,
                        "isolation": iso,
                        "active": false,
                        "quota": q,
                        "error": Value::Null,
                    }));
                } else {
                    inactive_host_ids.push(acc.clone());
                }
                continue;
            }

            let hub_c = hub.clone();
            let settings_c = settings.clone();
            let acc_c = acc.clone();
            let sem_c = sem.clone();
            isolated_tasks.spawn(async move {
                let _permit = sem_c.acquire_owned().await.ok();
                let res = quota(&hub_c, tool, &settings_c, Some(&acc_c.id), refresh).await;
                (acc_c, iso, is_active, res)
            });
        }

        while let Some(joined) = isolated_tasks.join_next().await {
            if let Ok((acc, iso, is_active, res)) = joined {
                let (q, err) = match res {
                    Ok(q) => (Some(q), None),
                    Err(e) => (
                        cached_quota(tool, &acc.id, hub.accounts.base_dir()),
                        Some(e.to_string()),
                    ),
                };
                results.insert(acc.id.clone(), json!({
                    "accountId": acc.id,
                    "label": acc.label,
                    "authKind": acc.auth_kind,
                    "isolation": iso,
                    "active": is_active,
                    "quota": q,
                    "error": err,
                }));
            }
        }

        // Sequential snapshot swap probe for inactive HostActive accounts when explicitly requested.
        if !inactive_host_ids.is_empty() {
            if hub.has_running_host_session(tool) {
                for acc in inactive_host_ids {
                    results.insert(acc.id.clone(), json!({
                        "accountId": acc.id,
                        "label": acc.label,
                        "authKind": acc.auth_kind,
                        "isolation": AccountIsolation::HostActive,
                        "active": false,
                        "quota": cached_quota(tool, &acc.id, hub.accounts.base_dir()),
                        "error": "当前有活跃会话正在运行，已跳过切换探测",
                    }));
                }
            } else {
                let orig_active = active_id.clone();
                let mut swapped = false;
                for acc in inactive_host_ids {
                    if hub.accounts.activate(tool, &acc.id).is_ok() {
                        swapped = true;
                        let res = quota(hub, tool, &settings, Some(&acc.id), true).await;
                        let (q, err) = match res {
                            Ok(q) => (Some(q), None),
                            Err(e) => (
                                cached_quota(tool, &acc.id, hub.accounts.base_dir()),
                                Some(e.to_string()),
                            ),
                        };
                        results.insert(acc.id.clone(), json!({
                            "accountId": acc.id,
                            "label": acc.label,
                            "authKind": acc.auth_kind,
                            "isolation": AccountIsolation::HostActive,
                            "active": false,
                            "quota": q,
                            "error": err,
                        }));
                    } else {
                        results.insert(acc.id.clone(), json!({
                            "accountId": acc.id,
                            "label": acc.label,
                            "authKind": acc.auth_kind,
                            "isolation": AccountIsolation::HostActive,
                            "active": false,
                            "quota": cached_quota(tool, &acc.id, hub.accounts.base_dir()),
                            "error": "无法切换该账号凭据以探测额度",
                        }));
                    }
                }
                if swapped {
                    if let Some(orig) = orig_active {
                        let _ = hub.accounts.activate(tool, &orig);
                    }
                }
            }
        }

        let ordered: Vec<Value> = accounts
            .iter()
            .filter_map(|a| results.remove(&a.id))
            .collect();
        out_tools.push(json!({
            "tool": tool.as_str(),
            "oauthIsolation": cap.oauth_isolation,
            "activeAccountId": active_id,
            "accounts": ordered,
            "nativeQuota": Value::Null,
        }));
    }

    Ok(json!({ "tools": out_tools }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cut from `agy -p /usage --output-format json` (1.5.7)
    #[test]
    fn antigravity_quota_shape() {
        let usage = json!({"groups":[
            {"name":"Gemini Models","buckets":[
                {"id":"gemini-weekly","window":"weekly","remaining_fraction":0.9897445440292358,"reset_time":"2026-11-19T22:11:03Z"},
                {"id":"gemini-5h","window":"5h","remaining_fraction":0.9891058206558228,"reset_time":"2026-11-13T03:11:03Z"}]},
            {"name":"Claude and GPT models","buckets":[
                {"id":"3p-5h","window":"5h","remaining_fraction":0,"reset_time":"2026-11-13T03:16:47Z"}]}]});
        let q = antigravity_quota(&usage, &json!({"remaining_credits":0}));
        assert_eq!(q["windows"][0]["label"], "Gemini 每周");
        assert_eq!(q["windows"][0]["usedPercent"], 1.0);
        assert_eq!(q["windows"][1]["label"], "Gemini 5 小时");
        assert_eq!(q["windows"][2]["label"], "Claude / GPT 5 小时");
        assert_eq!(q["windows"][2]["usedPercent"], 100.0);
        assert_eq!(q["windows"][2]["resetsAt"], "2026-11-13T03:16:47Z");
        assert!(q["note"].is_null());
        assert_eq!(antigravity_quota(&usage, &json!({"remaining_credits":250}))["note"], "AI 积分剩余 250");
    }

    #[test]
    fn cursor_quota_shape() {
        let usage = json!({"billingCycleEnd":"1794881855000","planUsage":{"includedSpend":807,"limit":2000,
            "autoPercentUsed":0,"apiPercentUsed":20.175,"totalPercentUsed":3.228},
            "spendLimitUsage":{"individualLimit":0,"limitType":"team"}});
        let plan = json!({"planInfo":{"planName":"Team"}});
        let q = cursor_quota(&usage, &plan, &json!({"hardLimit":5000}));
        assert_eq!(q["account"]["plan"], "Team");
        assert_eq!(q["windows"][0]["usedPercent"], 3.228);
        assert_eq!(q["windows"][0]["resetsAt"], 1794881855000i64);
        // Team account with a personal limit of 0: pay-as-you-go not enabled
        assert_eq!(q["windows"][3]["detail"], "未开启");
        let q = cursor_quota(&json!({"spendLimitUsage":{"individualUsed":1234}}), &plan, &json!({"hardLimit":50}));
        assert_eq!(q["windows"][0]["detail"], "$12.34 / $50.00");
    }

    #[test]
    fn quota_shapes() {
        let c = claude_quota(
            &json!({"email":"a@b","subscriptionType":"Claude Pro"}),
            &json!({"rate_limits_available":true,"rate_limits":{
                "five_hour":{"utilization":83,"resets_at":"2026-10-28T05:29:59Z"},
                "seven_day":{"utilization":54,"resets_at":"2026-11-01T07:59:59Z"},
                "seven_day_opus":null}}),
        );
        assert_eq!(c["windows"].as_array().unwrap().len(), 2);
        assert_eq!(c["windows"][0]["label"], "5 小时");
        assert!(c["note"].is_null());

        let x = codex_quota(
            &json!({"email":"a@b","planType":"plus"}),
            &json!({"primary":{"usedPercent":0,"windowDurationMins":300,"resetsAt":1793177589},
                    "secondary":{"usedPercent":63,"windowDurationMins":10080,"resetsAt":1793604850}}),
        );
        assert_eq!(x["windows"][1]["label"], "每周");
        assert_eq!(x["windows"][1]["resetsAt"], 1793604850000i64);

        let k = kiro_quota(&json!({"success":true,"data":{"planName":"KIRO PRO","billingCycleReset":"2026-11-01",
            "usageBreakdowns":[{"displayName":"Credits","used":987.76,"limit":1000.0,"percentage":98.776,"hasLimit":true}]}}));
        assert_eq!(k["windows"][0]["detail"], "987.76 / 1000");
        assert_eq!(k["account"]["plan"], "KIRO PRO");

        let q = qoder_quota(
            &json!({"email":"u@qoder.com","subscriptionType":"Pro"}),
            &json!({"usage":{"expiresAt":1794672000000i64,"userQuota":{"total":2000,"used":1096,"percentage":55},
                "addOnQuota":{"total":3100,"used":2200,"percentage":71},
                "dedicatedResourcePackages":[{"name":"act","total":1024,"used":415,"percentage":41,
                    "expiresAt":1793462339000i64,"available":true,
                    "displayLabels":[{"dimension":"title","valueI18n":{"zh-CN":"SOTA 专属积分"}}]}]}}),
        );
        assert_eq!(q["account"]["plan"], "Pro");
        assert_eq!(q["windows"].as_array().unwrap().len(), 3);
        assert_eq!(q["windows"][0]["label"], "套餐积分");
        assert_eq!(q["windows"][0]["detail"], "1096 / 2000");
        assert_eq!(q["windows"][1]["label"], "增购积分");
        assert_eq!(q["windows"][2]["label"], "SOTA 专属积分");

        let cb = codebuddy_quota(
            &json!({"userNickname":"李迪"}),
            &json!({"data":{"SubscriptionPackageName":"体验版","CreditGroups":[
                {"Name":"套餐基础积分","CycleTotalCapacity":"500","CycleUsedCapacity":"500"},
                {"Name":"购买积分","CycleTotalCapacity":"0","CycleUsedCapacity":"0"},
                {"Name":"平台奖励积分","CycleTotalCapacity":"4800","CycleUsedCapacity":"2561.73"}
            ]}}),
            &Value::Null,
        );
        assert_eq!(cb["account"]["email"], "李迪");
        assert_eq!(cb["account"]["plan"], "体验版");
        assert_eq!(cb["windows"].as_array().unwrap().len(), 2);
        assert_eq!(cb["windows"][0]["detail"], "500 / 500");
        assert_eq!(cb["windows"][1]["detail"], "2561.73 / 4800");
    }

    #[test]
    fn cursor_auth_helpers_detect_api_key_and_endpoint() {
        assert_eq!(
            cursor_auth_api_key(br#"{"accessToken":"t","apiKey":"key_abc"}"#).as_deref(),
            Some("key_abc")
        );
        assert_eq!(
            cursor_auth_api_key(br#"{"accessToken":"t"}"#),
            None
        );
        assert_eq!(
            cursor_token_from_bytes(br#"{"accessToken":"tok_1"}"#).unwrap(),
            "tok_1"
        );
        let mut penv = ProbeEnv::default();
        penv.env
            .insert("CURSOR_API_ENDPOINT".into(), "https://custom.cursor.example/".into());
        assert_eq!(cursor_api_endpoint(&penv), "https://custom.cursor.example");
    }
}

/// Check an API key profile against its endpoint's models list; returns how many models it lists.
/// Claude: `/v1/models` (x-api-key, or Bearer for an auth token); Codex: OpenAI-style `/models` (Bearer);
/// Antigravity: Gemini `/v1beta/models` (x-goog-api-key). Other tools have no endpoint we can check.
pub fn test_api_key(tool: ToolKind, cfg: &ApiKeyConfig) -> Result<usize> {
    let key = cfg.api_key.trim();
    let token = cfg.auth_token.trim();
    if key.is_empty() && token.is_empty() {
        bail!("没有填写 API Key");
    }
    let base = cfg.base_url.trim().trim_end_matches('/');
    let with_v1 = |default: &str| {
        let b = if base.is_empty() { default } else { base };
        if b.ends_with("/v1") { format!("{b}/models") } else { format!("{b}/v1/models") }
    };
    let req = match tool {
        ToolKind::Claude => {
            let r = ureq::get(&with_v1("https://api.anthropic.com")).header("anthropic-version", "2023-06-01");
            if key.is_empty() { r.header("Authorization", &format!("Bearer {token}")) } else { r.header("x-api-key", key) }
        }
        ToolKind::Codex => {
            let b = if base.is_empty() { "https://api.openai.com/v1" } else { base };
            ureq::get(&format!("{b}/models")).header("Authorization", &format!("Bearer {}", if key.is_empty() { token } else { key }))
        }
        ToolKind::Antigravity => {
            let b = if base.is_empty() { "https://generativelanguage.googleapis.com" } else { base };
            ureq::get(&format!("{b}/v1beta/models")).header("x-goog-api-key", key)
        }
        _ => bail!("{} 不支持测试连接", tool.as_str()),
    };
    let resp = req.config().timeout_global(Some(Duration::from_secs(15))).build().call();
    let mut resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(401 | 403)) => bail!("Key 无效或没有权限（HTTP 401/403）"),
        Err(ureq::Error::StatusCode(404)) => bail!("接口地址不对：没有找到模型列表（HTTP 404）"),
        Err(ureq::Error::StatusCode(code)) => bail!("接口返回 HTTP {code}"),
        Err(e) => bail!("连接失败：{e}"),
    };
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    let v: Value = serde_json::from_str(&body).map_err(|_| anyhow!("接口返回的不是模型列表"))?;
    let n = v["data"].as_array().or_else(|| v["models"].as_array()).map(|a| a.len());
    n.ok_or_else(|| anyhow!("接口返回的不是模型列表"))
}
