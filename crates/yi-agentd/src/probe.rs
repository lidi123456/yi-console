//! Start a temporary tool process to query the model list and account quota without creating a session (ACP tools need an empty session, created in a dedicated directory).

use crate::adapters::{self, JsonProc};
use crate::hub::Hub;
use crate::util::now_ms;
use anyhow::{anyhow, bail, Result};
use yi_proto::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// Start the process, send first, then pass each line received to on until Done or timeout
async fn converse(
    tool: ToolKind,
    settings: &DaemonSettings,
    args: &[&str],
    first: Vec<Value>,
    mut on: impl FnMut(&Value) -> Step,
) -> Result<()> {
    let program = crate::tools::resolve(tool, settings)
        .ok_or_else(|| anyhow!("这台机器上没有找到 {}", tool.as_str()))?;
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let mut p = JsonProc::spawn(&program, &args, &probe_dir())?;
    for m in first {
        p.send(&m).await?;
    }
    let r = tokio::time::timeout(TIMEOUT, async {
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

async fn claude(settings: &DaemonSettings, want_quota: bool) -> Result<Probe> {
    let mut out = Probe::default();
    let mut account = Value::Null;
    let init = json!({"type":"control_request","request_id":"init","request":{"subtype":"initialize"}});
    converse(
        ToolKind::Claude,
        settings,
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

async fn codex(settings: &DaemonSettings, want_quota: bool) -> Result<Probe> {
    let mut out = Probe::default();
    let (mut account, mut limits) = (Value::Null, Value::Null);
    let mut waiting = 0;
    let init = json!({"id":1,"method":"initialize",
        "params":{"clientInfo":{"name":"yi-console","title":"Yi Console","version":DAEMON_VERSION}}});
    converse(ToolKind::Codex, settings, &["app-server"], vec![init], |v| {
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
            2 => account = r["account"].clone(),
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

/// Cursor / Kiro: create an empty session to get the model list; Kiro then runs /usage
async fn acp(tool: ToolKind, settings: &DaemonSettings, want_quota: bool) -> Result<Probe> {
    let mut out = Probe::default();
    let cwd = probe_dir().display().to_string();
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,
        "clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},
        "clientInfo":{"name":"yi-console","version":DAEMON_VERSION}}});
    let kiro_usage = want_quota && tool == ToolKind::Kiro;
    let mut sid = String::new();
    // Every session Qoder creates shows up in its history, so delete it after asking
    let delete = tool == ToolKind::Qoder;
    converse(tool, settings, &[adapters::acp::acp_arg(tool)], vec![init], |v| {
        let Some(id) = v["id"].as_u64().filter(|_| v.get("method").is_none()) else {
            return Step::Wait;
        };
        let r = &v["result"];
        match id {
            1 => Step::Send(vec![json!({"jsonrpc":"2.0","id":2,"method":"session/new",
                "params":{"cwd":cwd,"mcpServers":[]}})]),
            2 => {
                out.models = adapters::acp::parse_models(r);
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

fn fmt_num(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{x:.0}")
    } else {
        format!("{x:.2}")
    }
}

/// Cursor: account from `cursor-agent about`, usage queried from Cursor's backend (same as cursor-agent's /usage)
async fn cursor_account(settings: &DaemonSettings) -> Result<Value> {
    let program = crate::tools::resolve(ToolKind::Cursor, settings)
        .ok_or_else(|| anyhow!("这台机器上没有找到 cursor"))?;
    let mut cmd: tokio::process::Command =
        adapters::std_command(&program, &["about".to_string()]).into();
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
    match tokio::task::spawn_blocking(cursor_usage).await? {
        Ok(mut q) => {
            if q["account"]["plan"].is_null() {
                q["account"]["plan"] = json!(tier);
            }
            q["account"]["email"] = json!(email);
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

fn cursor_call(token: &str, method: &str) -> Result<Value> {
    let url = format!("https://api2.cursor.sh/aiserver.v1.DashboardService/{method}");
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

fn cursor_usage() -> Result<Value> {
    let path = cursor_auth_file();
    let auth: Value = serde_json::from_slice(&std::fs::read(&path).map_err(|_| anyhow!("没有找到 cursor-agent 的登录信息"))?)?;
    let token = auth["accessToken"].as_str().filter(|t| !t.is_empty()).ok_or_else(|| anyhow!("cursor-agent 还没有登录"))?;
    let usage = cursor_call(token, "GetCurrentPeriodUsage")?;
    let plan = cursor_call(token, "GetPlanInfo").unwrap_or(Value::Null);
    let hard = cursor_call(token, "GetHardLimit").unwrap_or(Value::Null);
    Ok(cursor_quota(&usage, &plan, &hard))
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
async fn qoder_account(settings: &DaemonSettings) -> Result<Value> {
    let mut account = Value::Null;
    let mut quota = Value::Null;
    let init = json!({"type":"control_request","request_id":"init","request":{"subtype":"initialize"}});
    converse(
        ToolKind::Qoder,
        settings,
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

fn codebuddy_call(base: &str, path: &str, token: &str, ent_id: Option<&str>) -> Result<Value> {
    let url = format!("{base}{path}");
    let mut req = ureq::post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {token}"));
    if let Some(eid) = ent_id.filter(|s| !s.is_empty()) {
        req = req.header("X-Enterprise-Id", eid);
    }
    let mut resp = req
        .config()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .send(&b"{}"[..])
        .map_err(|e| match e {
            ureq::Error::StatusCode(401 | 403) => anyhow!("登录已过期，请运行一次 codebuddy 重新登录"),
            e => anyhow!("{e}"),
        })?;
    Ok(serde_json::from_str(&resp.body_mut().read_to_string()?)?)
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
    let summary = codebuddy_call(base, "/billing/meter/get-user-resource-summary", token, ent_id).unwrap_or(Value::Null);
    if ent["data"].is_null() && summary["data"].is_null() {
        bail!("后台没有返回额度数据");
    }
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

async fn run(tool: ToolKind, settings: &DaemonSettings, want_quota: bool) -> Result<Probe> {
    match tool {
        ToolKind::Claude => claude(settings, want_quota).await,
        ToolKind::Codex => codex(settings, want_quota).await,
        ToolKind::Kiro => acp(tool, settings, want_quota).await,
        ToolKind::Qoder if want_quota => Ok(Probe {
            models: vec![],
            quota: Some(qoder_account(settings).await?),
        }),
        ToolKind::Codebuddy if want_quota => Ok(Probe {
            models: vec![],
            quota: Some(codebuddy_account(settings).await?),
        }),
        ToolKind::Qoder | ToolKind::Codebuddy => acp(tool, settings, false).await,
        ToolKind::Cursor if want_quota => Ok(Probe {
            models: vec![],
            quota: Some(cursor_account(settings).await?),
        }),
        ToolKind::Cursor => acp(tool, settings, false).await,
        ToolKind::Shell => bail!("终端没有模型和额度"),
        ToolKind::Files => bail!("文件会话没有模型和额度"),
    }
}

pub async fn models(tool: ToolKind, settings: &DaemonSettings) -> Result<Vec<ModelOption>> {
    Ok(run(tool, settings, false).await?.models)
}

/// Account quota: {tool, account:{email,plan}, windows:[{label,usedPercent,resetsAt,detail?}], note?, fetchedAt}
pub async fn quota(hub: &Arc<Hub>, tool: ToolKind, settings: &DaemonSettings, refresh: bool) -> Result<Value> {
    let key = tool.as_str().to_string();
    if !refresh {
        let cache = QUOTA_CACHE.lock().unwrap();
        if let Some((at, v)) = cache.as_ref().and_then(|m| m.get(&key)) {
            if now_ms() - at < QUOTA_TTL_MS {
                return Ok(v.clone());
            }
        }
    }
    let p = run(tool, settings, true).await?;
    hub.remember_models(tool, &p.models);
    let mut v = p.quota.ok_or_else(|| anyhow!("{} 没有返回额度信息", tool.as_str()))?;
    let now = now_ms();
    v["tool"] = json!(key);
    v["fetchedAt"] = json!(now);
    QUOTA_CACHE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(key, (now, v.clone()));
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
