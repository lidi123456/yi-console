//! 用量统计：按天、按工具聚合。
//!
//! Claude Code 和 Codex 自己会把每次请求的 token 记在本地日志里（`~/.claude/projects`、
//! `~/.codex/sessions`），直接在终端里用的也在，所以这两个工具的 token 以日志为准；
//! 经 Yi Console 跑的会话也会写这些日志，不再重复算。Cursor / Kiro 没有本地记录，用 Yi Console 自己的 usage_log。
//! 费用只有 Claude Code 在 Yi Console 会话里上报，来自 usage_log。

use crate::store::Store;
use crate::util::{day_of, now_ms, parse_iso};
use yi_proto::Usage;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// 日志里的一次请求
#[derive(Clone)]
struct Rec {
    ts: i64,
    model: String,
    usage: Usage,
    /// Claude 同一条消息会按内容块重复写多行，按 message.id + requestId 去重
    key: Option<String>,
}

/// 解析过的日志文件：(修改时间, 大小, 记录)，文件没变就不再读
type Cache = HashMap<PathBuf, (SystemTime, u64, Arc<Vec<Rec>>)>;
static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn cached(path: &Path, parse: fn(&str) -> Vec<Rec>) -> Arc<Vec<Rec>> {
    let meta = std::fs::metadata(path).ok();
    let stamp = meta.as_ref().map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
    if let Some((mt, len)) = stamp {
        if let Some((m2, l2, v)) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(path) {
            if *m2 == mt && *l2 == len {
                return v.clone();
            }
        }
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let recs = Arc::new(parse(&text));
    if let Some((mt, len)) = stamp {
        CACHE
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(path.to_path_buf(), (mt, len, recs.clone()));
    }
    recs
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

fn parse_claude(text: &str) -> Vec<Rec> {
    let mut out = vec![];
    for line in text.lines() {
        // 先粗筛，整行 JSON 解析比较慢
        if !line.contains("\"usage\"") || !line.contains("\"assistant\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v["type"] != "assistant" {
            continue;
        }
        let m = &v["message"];
        let us = &m["usage"];
        if !us.is_object() {
            continue;
        }
        let Some(ts) = v["timestamp"].as_str().and_then(parse_iso) else { continue };
        let model = m["model"].as_str().unwrap_or("").to_string();
        // 本地合成的消息（报错提示等）不算
        if model == "<synthetic>" {
            continue;
        }
        let key = match (m["id"].as_str(), v["requestId"].as_str()) {
            (Some(a), Some(b)) => Some(format!("{a}:{b}")),
            (Some(a), None) => Some(a.to_string()),
            _ => None,
        };
        out.push(Rec {
            ts,
            model,
            key,
            usage: Usage {
                input_tokens: u(&us["input_tokens"]),
                output_tokens: u(&us["output_tokens"]),
                cache_read_tokens: u(&us["cache_read_input_tokens"]),
                cache_write_tokens: u(&us["cache_creation_input_tokens"]),
                cost_usd: 0.0,
                credits: 0.0,
            },
        });
    }
    out
}

/// Codex 的 token_count 事件带累计值，相邻两次的差就是这次请求的用量
fn parse_codex(text: &str) -> Vec<Rec> {
    let mut out = vec![];
    let mut model = String::new();
    let mut prev: Option<(u64, u64, u64)> = None;
    for line in text.lines() {
        let tc = line.contains("\"token_count\"");
        if !tc && !line.contains("\"turn_context\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let p = &v["payload"];
        if v["type"] == "turn_context" {
            if let Some(m) = p["model"].as_str() {
                model = m.to_string();
            }
            continue;
        }
        if p["type"] != "token_count" {
            continue;
        }
        let t = &p["info"]["total_token_usage"];
        if !t.is_object() {
            continue;
        }
        let Some(ts) = v["timestamp"].as_str().and_then(parse_iso) else { continue };
        let cur = (u(&t["input_tokens"]), u(&t["cached_input_tokens"]), u(&t["output_tokens"]));
        let base = match prev {
            // 累计值变小说明换了线程（压缩上下文等），从头算
            Some(b) if cur.0 >= b.0 && cur.2 >= b.2 => b,
            _ => (0, 0, 0),
        };
        prev = Some(cur);
        let (input, cached, output) = (cur.0 - base.0, cur.1.saturating_sub(base.1), cur.2 - base.2);
        if input == 0 && output == 0 {
            continue;
        }
        out.push(Rec {
            ts,
            model: model.clone(),
            key: None,
            usage: Usage {
                // OpenAI 的 input_tokens 包含缓存命中的部分
                input_tokens: input.saturating_sub(cached),
                output_tokens: output,
                cache_read_tokens: cached,
                cache_write_tokens: u(&t["cache_write_input_tokens"]).min(input),
                cost_usd: 0.0,
                credits: 0.0,
            },
        });
    }
    out
}

fn qoder_model_name(m: &str) -> &str {
    match m {
        "auto" => "Auto",
        "ultimate" => "Ultimate",
        "fast" => "Fast",
        "efficient" => "Efficient",
        "smodel" => "Sonus",
        "cmodel" => "Cantus",
        "qmodel_38max" => "Qwen3.8-Max",
        "qfmodel" => "Qwen3.8-Flash",
        "qmodel_latest" => "Qwen3.7-Max",
        "qmodel" => "Qwen3.7-Plus",
        "q35model" => "Qwen3.5-Plus",
        "kmodel_latest" => "Kimi-K3.1-Pro",
        "kmodel" => "Kimi-K2.8-Preview",
        "gmodel" => "GLM-5.3",
        "gfmodel" => "GLM-5.3-Flash",
        "gm51model" => "GLM-5.2",
        "dmodel" => "DeepSeek-V4-Pro",
        "dfmodel" => "DeepSeek-Flash",
        "mmodel" => "MiniMax-M3.1-Flash",
        _ => m,
    }
}

/// Qoder 的会话日志和 Claude 同格式，但内置模型的 token 字段全置 0，用量记在 credits 里
fn parse_qoder(text: &str) -> Vec<Rec> {
    let mut out = vec![];
    for line in text.lines() {
        if !line.contains("\"usage\"") || !line.contains("\"assistant\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v["type"] != "assistant" {
            continue;
        }
        let m = &v["message"];
        let us = &m["usage"];
        if !us.is_object() {
            continue;
        }
        let Some(ts) = v["timestamp"].as_str().and_then(parse_iso) else { continue };
        let raw_model = m["model"].as_str().unwrap_or("");
        if raw_model == "<synthetic>" {
            continue;
        }
        let model = qoder_model_name(raw_model).to_string();
        let key = us["request_id"]
            .as_str()
            .or_else(|| v["requestTokenAnchor"]["requestId"].as_str())
            .map(str::to_string)
            .or_else(|| match (m["id"].as_str(), v["requestId"].as_str()) {
                (Some(a), Some(b)) => Some(format!("{a}:{b}")),
                (Some(a), None) => Some(a.to_string()),
                _ => None,
            });
        let credits = if us["billable"].as_bool() == Some(false) {
            0.0
        } else {
            us["credits"].as_f64().unwrap_or(0.0).max(0.0)
        };
        let input_tokens = u(&us["input_tokens"]);
        let output_tokens = u(&us["output_tokens"]);
        let cache_read_tokens = u(&us["cache_read_input_tokens"]);
        let cache_write_tokens = u(&us["cache_creation_input_tokens"]);
        if input_tokens == 0 && output_tokens == 0 && cache_read_tokens == 0 && cache_write_tokens == 0 && credits == 0.0 {
            continue;
        }
        out.push(Rec {
            ts,
            model,
            key,
            usage: Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                cost_usd: 0.0,
                credits,
            },
        });
    }
    out
}

/// CodeBuddy 的会话日志：assistant 消息的 providerData.rawUsage 里同时记了 token 和 credit
fn parse_codebuddy(text: &str) -> Vec<Rec> {
    let mut out = vec![];
    for line in text.lines() {
        if !line.contains("\"providerData\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let Some(ts) = v["timestamp"].as_i64().or_else(|| v["timestamp"].as_str().and_then(parse_iso)) else {
            continue;
        };
        let pd = &v["providerData"];
        if !pd.is_object() {
            continue;
        }
        if v["type"] == "credit-usage" {
            let credits = pd["credit"].as_f64().unwrap_or(0.0).max(0.0);
            if credits <= 0.0 {
                continue;
            }
            let key = pd["stageId"]
                .as_str()
                .map(|s| format!("{}:{s}", pd["conversationRequestId"].as_str().unwrap_or("")))
                .or_else(|| v["id"].as_str().map(str::to_string));
            out.push(Rec {
                ts,
                model: String::new(),
                key,
                usage: Usage {
                    credits,
                    ..Default::default()
                },
            });
            continue;
        }
        if v["type"] != "message" || v["role"] != "assistant" || pd["agent"] == "compact" {
            continue;
        }
        let ru = if pd["rawUsage"].is_object() { &pd["rawUsage"] } else { &pd["usage"] };
        if !ru.is_object() {
            continue;
        }
        let key = pd["messageId"]
            .as_str()
            .or_else(|| v["id"].as_str())
            .map(str::to_string);
        let raw_in = u(&ru["prompt_tokens"]).max(u(&ru["input_tokens"])).max(u(&ru["inputTokens"]));
        let output_tokens = u(&ru["completion_tokens"]).max(u(&ru["output_tokens"])).max(u(&ru["outputTokens"]));
        let first_pos = |vals: &[&Value]| vals.iter().find_map(|x| x.as_u64().filter(|&n| n > 0)).unwrap_or(0);
        let cache_read_tokens = first_pos(&[
            &ru["cache_read_input_tokens"],
            &ru["cacheReadInputTokens"],
            &ru["prompt_tokens_details"]["cached_tokens"],
            &ru["prompt_cache_hit_tokens"],
            &ru["inputTokensDetails"][0]["cached_tokens"],
        ]);
        let cache_write_tokens = first_pos(&[
            &ru["cache_creation_input_tokens"],
            &ru["cacheCreationInputTokens"],
            &ru["prompt_cache_write_tokens"],
        ]);
        // prompt_tokens 包含缓存部分（同 OpenAI 格式）
        let has_prompt = ru.get("prompt_tokens").is_some()
            || ru.get("prompt_tokens_details").is_some()
            || ru.get("prompt_cache_hit_tokens").is_some();
        let input_tokens = if has_prompt {
            raw_in.saturating_sub(cache_read_tokens).saturating_sub(cache_write_tokens)
        } else {
            raw_in
        };
        let credits = ru["credit"].as_f64().unwrap_or(0.0).max(0.0);
        if input_tokens == 0 && output_tokens == 0 && cache_read_tokens == 0 && cache_write_tokens == 0 && credits == 0.0 {
            continue;
        }
        let model = pd["requestModelId"]
            .as_str()
            .or_else(|| pd["model"].as_str())
            .or_else(|| pd["requestModelName"].as_str())
            .unwrap_or("")
            .to_string();
        out.push(Rec {
            ts,
            model,
            key,
            usage: Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                cost_usd: 0.0,
                credits,
            },
        });
    }
    out
}

/// 修改时间早于 since 的文件里不会有这段时间的记录
fn recent_files(root: &Path, since: i64) -> Vec<PathBuf> {
    let mut v = vec![];
    crate::history::walk_jsonl(root, &mut v, 0);
    v.retain(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64 >= since)
            .unwrap_or(true)
    });
    v
}

#[derive(Default)]
struct Agg {
    by_day: BTreeMap<(String, String), Usage>,
    totals: BTreeMap<String, Usage>,
    models: BTreeMap<String, BTreeMap<String, f64>>,
}

impl Agg {
    fn add(&mut self, tool: &str, day: String, model: &str, us: &Usage) {
        self.by_day.entry((day, tool.to_string())).or_default().add(us);
        self.totals.entry(tool.to_string()).or_default().add(us);
        if !model.is_empty() {
            let tokens = us.input_tokens + us.output_tokens + us.cache_read_tokens + us.cache_write_tokens;
            let v = if tokens > 0 { tokens as f64 } else { us.credits };
            if v > 0.0 {
                *self
                    .models
                    .entry(tool.to_string())
                    .or_default()
                    .entry(model.to_string())
                    .or_default() += v;
            }
        }
    }
}

pub fn usage(store: &Store, days: i64, tz_offset_min: i64) -> anyhow::Result<Value> {
    let since = now_ms() - days.max(1) * 86_400_000;
    let mut agg = Agg::default();
    let mut from_logs: Vec<&str> = vec![];

    let sources: [(&str, PathBuf, fn(&str) -> Vec<Rec>); 4] = [
        ("claude", crate::history::claude_root(), parse_claude),
        ("codex", crate::history::codex_root(), parse_codex),
        ("qoder", crate::history::qoder_root(), parse_qoder),
        ("codebuddy", crate::history::codebuddy_root(), parse_codebuddy),
    ];
    for (tool, root, parse) in sources {
        if !root.is_dir() {
            continue;
        }
        from_logs.push(tool);
        let mut seen = HashSet::new();
        for f in recent_files(&root, since) {
            for r in cached(&f, parse).iter() {
                if r.ts < since {
                    continue;
                }
                if let Some(k) = &r.key {
                    if !seen.insert(k.clone()) {
                        continue;
                    }
                }
                agg.add(tool, day_of(r.ts, tz_offset_min), &r.model, &r.usage);
            }
        }
    }

    // Yi Console 自己记的：日志里有的工具已包含 token 和积分，只取费用（Claude），其余全取
    for (ts, tool, mut us) in store.usage_rows(since)? {
        if from_logs.contains(&tool.as_str()) {
            us = Usage {
                cost_usd: us.cost_usd,
                ..Default::default()
            };
            if us.cost_usd == 0.0 {
                continue;
            }
        }
        agg.add(&tool, day_of(ts, tz_offset_min), "", &us);
    }

    let daily: Vec<Value> = agg
        .by_day
        .into_iter()
        .map(|((day, tool), usage)| json!({"day": day, "tool": tool, "usage": usage}))
        .collect();
    Ok(json!({ "daily": daily, "totals": agg.totals, "models": agg.models, "fromLogs": from_logs }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_dedupe_fields() {
        let line = r#"{"type":"assistant","timestamp":"2026-10-27T12:27:05.677Z","requestId":"r1","message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":2,"cache_creation_input_tokens":10,"cache_read_input_tokens":20,"output_tokens":30}}}"#;
        let text = format!("{line}\n{line}\n");
        let r = parse_claude(&text);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].key.as_deref(), Some("m1:r1"));
        assert_eq!(r[0].usage.cache_read_tokens, 20);
        assert_eq!(r[0].model, "claude-opus-5-5");
    }

    #[test]
    fn codex_deltas() {
        let tc = |i: u64, c: u64, o: u64| {
            format!(r#"{{"timestamp":"2026-10-28T04:13:33.094Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{i},"cached_input_tokens":{c},"output_tokens":{o}}}}}}}}}"#)
        };
        let text = [
            r#"{"timestamp":"2026-10-28T04:13:31.531Z","type":"turn_context","payload":{"model":"gpt-6.1-sol"}}"#.to_string(),
            tc(100, 80, 5),
            tc(100, 80, 5),
            tc(250, 200, 12),
        ]
        .join("\n");
        let r = parse_codex(&text);
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].usage.input_tokens, r[0].usage.cache_read_tokens, r[0].usage.output_tokens), (20, 80, 5));
        assert_eq!((r[1].usage.input_tokens, r[1].usage.cache_read_tokens, r[1].usage.output_tokens), (30, 120, 7));
        assert_eq!(r[1].model, "gpt-6.1-sol");
    }

    #[test]
    fn qoder_credits() {
        let line = r#"{"type":"assistant","timestamp":"2026-10-30T08:46:43.852Z","message":{"id":"m1","model":"smodel","usage":{"input_tokens":0,"output_tokens":0,"credits":0.35,"billable":true,"request_id":"req-1"}}}"#;
        let r = parse_qoder(line);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].model, "Sonus");
        assert_eq!(r[0].key.as_deref(), Some("req-1"));
        assert!((r[0].usage.credits - 0.35).abs() < 1e-6);
    }

    #[test]
    fn codebuddy_tokens_and_credits() {
        let line = r#"{"type":"message","role":"assistant","timestamp":1793350000000,"providerData":{"messageId":"msg-1","requestModelId":"glm-5.3","rawUsage":{"prompt_tokens":33154,"completion_tokens":109,"prompt_cache_hit_tokens":1408,"credit":5.2}}}"#;
        let r = parse_codebuddy(line);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].model, "glm-5.3");
        assert_eq!(r[0].usage.input_tokens, 33154 - 1408);
        assert_eq!(r[0].usage.cache_read_tokens, 1408);
        assert_eq!(r[0].usage.output_tokens, 109);
        assert!((r[0].usage.credits - 5.2).abs() < 1e-6);
    }
}
