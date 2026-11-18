//! Requests, routes, the domain allowlist and downloads.
//!
//! Interception (`Fetch`) runs on every tab, iframe and (with an allowlist) dedicated worker session; the browser
//! target's own `Fetch` doesn't see page requests. The allowlist follows agent-browser's design: requests are checked
//! there, and an init script guards what `Fetch` does not see (WebSocket, sendBeacon, WebRTC, worker scripts, service
//! worker registration). As agent-browser notes, the script is best effort: page code with `eval` can restore the
//! originals.

use crate::browser::{Browser, Inner};
use anyhow::{anyhow, bail, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) const REQUEST_LIMIT: usize = 500;

#[derive(Debug, Clone)]
pub struct Request {
    pub id: String,
    pub method: String,
    pub url: String,
    pub kind: String,
    pub status: Option<i64>,
    pub start: f64,
    pub ms: Option<f64>,
    pub failed: Option<String>,
    /// Where it came from, to read its body later
    pub session: String,
    pub req_headers: Value,
    pub post_data: Option<String>,
    pub has_post: bool,
    pub resp_headers: Value,
    pub mime: String,
    pub size: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Route {
    pub pattern: String,
    pub abort: bool,
    pub status: u16,
    pub body: String,
    pub content_type: String,
    pub headers: Vec<(String, String)>,
    /// Only requests of this type (Document, XHR, Fetch, Script, ...)
    pub resource_type: Option<String>,
}

/// Headers added to requests whose origin is `origin` (agent-browser's origin-scoped `--headers`)
#[derive(Debug, Clone)]
pub struct ScopedHeaders {
    pub origin: String,
    pub headers: Vec<(String, String)>,
}

/// HTTP basic / digest auth answer for an origin (None: any)
#[derive(Debug, Clone)]
pub struct Credentials {
    pub origin: Option<String>,
    pub username: String,
    pub password: String,
}

/// Header values the model shouldn't see: login tokens and cookies
const SECRET_HEADERS: &[&str] = &["authorization", "proxy-authorization", "cookie", "set-cookie", "x-api-key", "x-auth-token", "x-csrf-token", "x-xsrf-token"];

/// `name: value` lines of a CDP headers object, secrets replaced by their length
pub fn header_lines(h: &Value) -> Vec<String> {
    let mut v: Vec<(String, String)> = h
        .as_object()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect())
        .unwrap_or_default();
    v.sort_by(|a, b| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()));
    v.into_iter()
        .map(|(k, val)| {
            if SECRET_HEADERS.contains(&k.to_ascii_lowercase().as_str()) {
                format!("{k}: <已隐藏，{} 个字符>", val.chars().count())
            } else {
                format!("{k}: {}", crate::clip(&val, 300))
            }
        })
        .collect()
}

/// `404`, `4xx`, `>=400` style status filters
pub fn status_matches(filter: &str, status: Option<i64>) -> bool {
    let f = filter.trim().to_ascii_lowercase();
    let Some(s) = status else { return false };
    if let Some(n) = f.strip_prefix(">=").and_then(|n| n.trim().parse::<i64>().ok()) {
        return s >= n;
    }
    if f.len() == 3 && f.ends_with("xx") {
        return f[..1].parse::<i64>().is_ok_and(|d| s / 100 == d);
    }
    f.parse::<i64>().is_ok_and(|n| n == s)
}

/// Header value of a paused request, case-insensitively
fn header<'a>(h: &'a Value, name: &str) -> Option<&'a str> {
    h.as_object()?.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).and_then(|(_, v)| v.as_str())
}

/// Response headers of a mocked response: the route's own, a content type, and CORS for the request's origin
/// (echoed, with credentials, so a credentialed fetch from the dev server's origin passes too)
pub fn mock_headers(r: &Route, req_headers: &Value) -> Vec<(String, String)> {
    let mut h: Vec<(String, String)> = r.headers.clone();
    let has = |h: &Vec<(String, String)>, n: &str| h.iter().any(|(k, _)| k.eq_ignore_ascii_case(n));
    if !has(&h, "content-type") {
        h.push(("Content-Type".into(), r.content_type.clone()));
    }
    if !has(&h, "access-control-allow-origin") {
        match header(req_headers, "origin").filter(|o| *o != "null") {
            Some(o) => {
                h.push(("Access-Control-Allow-Origin".into(), o.to_string()));
                h.push(("Access-Control-Allow-Credentials".into(), "true".into()));
                h.push(("Vary".into(), "Origin".into()));
            }
            None => h.push(("Access-Control-Allow-Origin".into(), "*".into())),
        }
    }
    h
}

/// Answer to a CORS preflight for a mocked URL: allow what it asks for
pub fn preflight_headers(req_headers: &Value) -> Vec<(String, String)> {
    let origin = header(req_headers, "origin").unwrap_or("*").to_string();
    let mut h = vec![
        ("Access-Control-Allow-Origin".to_string(), origin.clone()),
        ("Access-Control-Allow-Methods".into(), header(req_headers, "access-control-request-method").unwrap_or("GET, POST, PUT, PATCH, DELETE").to_string()),
        ("Access-Control-Max-Age".into(), "600".into()),
        ("Vary".into(), "Origin".into()),
    ];
    if origin != "*" {
        h.push(("Access-Control-Allow-Credentials".into(), "true".into()));
    }
    if let Some(a) = header(req_headers, "access-control-request-headers") {
        h.push(("Access-Control-Allow-Headers".into(), a.to_string()));
    }
    h
}

fn cdp_headers(h: &[(String, String)]) -> Value {
    Value::Array(h.iter().map(|(k, v)| json!({"name": k, "value": v})).collect())
}

/// The page shown instead of a document outside the allowlist
fn blocked_page(url: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    format!(
        "<!doctype html><meta charset=utf-8><title>Blocked</title><body style=\"font:15px sans-serif;padding:40px;color:#333\"><h2>Yi Console blocked this page</h2><p>{}</p><p>It is not in the allowed domains set for the AI browser on this host.</p></body>",
        esc(&crate::clip(url, 300))
    )
}

#[derive(Debug, Clone)]
pub struct Download {
    pub guid: String,
    pub url: String,
    pub name: String,
    pub state: String,
    pub path: Option<PathBuf>,
    /// Reported to the model by browser_download or browser_wait already
    pub claimed: bool,
}

/// `example.com` matches that host; `*.example.com` matches it and every subdomain
pub fn domain_allowed(allowed: &[String], url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else { return true };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https" | "ws" | "wss") {
        return true;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority.rsplit('@').next().unwrap_or("");
    let host = if let Some(h) = hostport.strip_prefix('[') { h.split(']').next().unwrap_or("") } else { hostport.split(':').next().unwrap_or("") };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    allowed.iter().map(|p| p.trim().to_ascii_lowercase()).filter(|p| !p.is_empty()).any(|p| match p.strip_prefix("*.") {
        Some(base) => host == base || host.ends_with(&format!(".{base}")),
        None => host == p,
    })
}

/// Runs in every document (and, through a wrapper, in workers) while an allowlist is set
pub(crate) fn guard_script(allowed: &[String]) -> String {
    let list = serde_json::to_string(allowed).unwrap_or_else(|_| "[]".into());
    let core = format!(
        r#"const A={list};
const ok=u=>{{try{{const x=new URL(u,self.location.href);if(!/^(https?|wss?):$/.test(x.protocol))return true;const h=x.hostname.replace(/\.$/,'').toLowerCase().replace(/^\[|\]$/g,'');return A.some(p=>{{p=p.trim().toLowerCase();return p.startsWith('*.')?(h===p.slice(2)||h.endsWith(p.slice(1))):h===p}})}}catch(e){{return false}}}};
const deny=()=>new DOMException('Blocked by the Yi Console allowed domains','SecurityError');
const wrapCtor=(name)=>{{const C=self[name];if(!C)return;const W=function(u,...r){{if(!ok(String(u)))throw deny();return new C(u,...r)}};W.prototype=C.prototype;for(const k of ['CONNECTING','OPEN','CLOSING','CLOSED'])if(k in C)Object.defineProperty(W,k,{{value:C[k]}});Object.defineProperty(self,name,{{value:W,configurable:true,writable:true}})}};
wrapCtor('WebSocket');wrapCtor('EventSource');
if(self.navigator&&navigator.sendBeacon){{const b=navigator.sendBeacon.bind(navigator);navigator.sendBeacon=(u,d)=>ok(String(u))?b(u,d):false}}
for(const k of ['RTCPeerConnection','webkitRTCPeerConnection','RTCDataChannel'])try{{Object.defineProperty(self,k,{{value:undefined,configurable:false,writable:false}})}}catch(e){{}}
try{{if(self.navigator&&'serviceWorker' in navigator)Object.defineProperty(Navigator.prototype,'serviceWorker',{{get:()=>undefined,configurable:false}})}}catch(e){{}}"#
    );
    let core_json = serde_json::to_string(&core).unwrap_or_default();
    // A worker starts from a blob that installs the same guard and then loads the real script; when the blob can't
    // load (CSP), the worker fails instead of running unguarded
    format!(
        r#"(()=>{{{core}
const G={core_json};
const wrapWorker=(name)=>{{const C=self[name];if(!C)return;const W=function(u,o){{const url=new URL(String(u),self.location.href).href;if(!ok(url))throw deny();const mod=o&&typeof o==='object'&&o.type==='module';const src='(()=>{{'+G+'}})();'+(mod?'await import('+JSON.stringify(url)+');':'importScripts('+JSON.stringify(url)+');');return new C(URL.createObjectURL(new Blob([src],{{type:'text/javascript'}})),o)}};W.prototype=C.prototype;Object.defineProperty(self,name,{{value:W,configurable:true,writable:true}})}};
wrapWorker('Worker');wrapWorker('SharedWorker');
}})()"#
    )
}

fn sanitize(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_control() || "\\/:*?\"<>|".contains(c) { '_' } else { c }).collect();
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() { "download".into() } else { s.chars().take(150).collect() }
}

/// `name`, or `name (1).ext` ... when taken
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (1..10_000).map(|i| dir.join(format!("{stem} ({i}){ext}"))).find(|p| !p.exists()).unwrap_or(p)
}

impl Inner {
    /// Network.* events of a tab or iframe session
    pub(crate) fn on_network(&self, session: &str, method: &str, p: &Value) {
        let mut st = self.state.lock().unwrap();
        let Some(target) = st.session_tab.get(session).cloned() else { return };
        if st.tab(&target).is_none() {
            return;
        }
        if method == "Network.responseReceived" && p["type"] == "Document" {
            let main = st.tab(&target).map(|t| t.main_frame.clone()).unwrap_or_default();
            if p["frameId"].as_str() == Some(main.as_str()) {
                let code = p["response"]["status"].as_i64();
                if let Some(t) = st.tabs.iter_mut().find(|t| t.target_id == target) {
                    t.doc_status = code;
                }
            }
        }
        let requests = st.requests.entry(target).or_default();
        let id = p["requestId"].as_str().unwrap_or("");
        if method == "Network.requestWillBeSent" {
            let url = p["request"]["url"].as_str().unwrap_or("");
            if url.starts_with("data:") {
                return;
            }
            // A redirect reuses the request id: the earlier hop stays in the log as its own entry
            if let Some(prev) = requests.iter_mut().rev().find(|r| r.id == id && r.status.is_none()) {
                prev.status = p["redirectResponse"]["status"].as_i64();
                prev.resp_headers = p["redirectResponse"]["headers"].clone();
            }
            requests.push_back(Request {
                id: id.into(),
                method: p["request"]["method"].as_str().unwrap_or("GET").into(),
                url: crate::clip(url, 500),
                kind: p["type"].as_str().unwrap_or("Other").into(),
                status: None,
                start: p["timestamp"].as_f64().unwrap_or(0.0),
                ms: None,
                failed: None,
                session: session.into(),
                req_headers: p["request"]["headers"].clone(),
                post_data: p["request"]["postData"].as_str().map(|s| crate::clip(s, 10_000)),
                has_post: p["request"]["hasPostData"].as_bool().unwrap_or(false),
                resp_headers: Value::Null,
                mime: String::new(),
                size: None,
            });
            while requests.len() > REQUEST_LIMIT {
                requests.pop_front();
            }
            return;
        }
        let Some(r) = requests.iter_mut().rev().find(|r| r.id == id) else { return };
        match method {
            "Network.responseReceived" => {
                r.status = p["response"]["status"].as_i64();
                r.resp_headers = p["response"]["headers"].clone();
                r.mime = p["response"]["mimeType"].as_str().unwrap_or("").into();
            }
            "Network.loadingFinished" | "Network.loadingFailed" => {
                r.size = p["encodedDataLength"].as_f64().or(r.size);
                if let Some(t) = p["timestamp"].as_f64() {
                    r.ms = Some(((t - r.start) * 1000.0).max(0.0));
                }
                if method == "Network.loadingFailed" {
                    r.failed = Some(p["errorText"].as_str().unwrap_or("failed").into());
                }
            }
            _ => {}
        }
    }

    /// Fetch.requestPaused: allowlist first, then routes (with their CORS preflights), then origin-scoped headers,
    /// else continue
    pub(crate) fn on_paused(&self, session: &str, p: &Value) {
        let s = Some(session);
        let id = p["requestId"].clone();
        let url = p["request"]["url"].as_str().unwrap_or("");
        let kind = p["resourceType"].as_str().unwrap_or("");
        let headers = &p["request"]["headers"];
        if !self.allowed.is_empty() && !domain_allowed(&self.allowed, url) {
            let host = crate::tools::origin(url);
            let document = kind == "Document";
            if document {
                // A page saying why, instead of Chrome's generic error page
                let body = base64::engine::general_purpose::STANDARD.encode(blocked_page(url));
                self.cdp.notify(s, "Fetch.fulfillRequest", json!({"requestId": id, "responseCode": 403, "responseHeaders": [{"name": "Content-Type", "value": "text/html; charset=utf-8"}], "body": body}));
            } else {
                self.cdp.notify(s, "Fetch.failRequest", json!({"requestId": id, "errorReason": "BlockedByClient"}));
            }
            let mut st = self.state.lock().unwrap();
            if document || !st.blocked.contains(&host) {
                if !st.blocked.contains(&host) {
                    st.blocked.push(host.clone());
                }
                st.notes.push(if document {
                    format!("已拦截跳转到 {}：不在这台主机设置的允许域名列表里", crate::clip(url, 200))
                } else {
                    format!("已拦截不在允许域名列表里的请求：{host}")
                });
            }
            return;
        }
        let method = p["request"]["method"].as_str().unwrap_or("GET");
        let (route, scoped) = {
            let st = self.state.lock().unwrap();
            let route = st
                .routes
                .iter()
                .find(|r| crate::url_matches(&r.pattern, url) && r.resource_type.as_deref().is_none_or(|t| t.eq_ignore_ascii_case(kind) || (method == "OPTIONS" && kind == "Preflight")))
                .cloned();
            let origin = crate::tools::origin(url);
            let scoped: Vec<(String, String)> = st.scoped_headers.iter().filter(|h| h.origin.eq_ignore_ascii_case(&origin)).flat_map(|h| h.headers.clone()).collect();
            (route, scoped)
        };
        match route {
            Some(r) if r.abort => self.cdp.notify(s, "Fetch.failRequest", json!({"requestId": id, "errorReason": "Failed"})),
            Some(_) if method == "OPTIONS" && header(headers, "access-control-request-method").is_some() => {
                self.cdp.notify(s, "Fetch.fulfillRequest", json!({"requestId": id, "responseCode": 204, "responseHeaders": cdp_headers(&preflight_headers(headers))}));
            }
            Some(r) => self.cdp.notify(
                s,
                "Fetch.fulfillRequest",
                json!({
                    "requestId": id, "responseCode": r.status,
                    "responseHeaders": cdp_headers(&mock_headers(&r, headers)),
                    "body": base64::engine::general_purpose::STANDARD.encode(r.body.as_bytes()),
                }),
            ),
            None if !scoped.is_empty() => {
                // continueRequest replaces the headers, so send the original ones with ours merged in
                let mut all: Vec<(String, String)> = headers
                    .as_object()
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect())
                    .unwrap_or_default();
                for (k, v) in scoped {
                    all.retain(|(n, _)| !n.eq_ignore_ascii_case(&k));
                    all.push((k, v));
                }
                self.cdp.notify(s, "Fetch.continueRequest", json!({"requestId": id, "headers": cdp_headers(&all)}));
            }
            None => self.cdp.notify(s, "Fetch.continueRequest", json!({"requestId": id})),
        }
    }

    /// Fetch.authRequired: answer with credentials set for that origin, else cancel and tell the model
    pub(crate) fn on_auth(&self, session: &str, p: &Value) {
        let url = p["request"]["url"].as_str().unwrap_or("");
        let origin = crate::tools::origin(url);
        let mut st = self.state.lock().unwrap();
        let c = st.credentials.iter().find(|c| c.origin.as_deref().is_none_or(|o| o.eq_ignore_ascii_case(&origin))).cloned();
        let answer = match &c {
            Some(c) => json!({"response": "ProvideCredentials", "username": c.username, "password": c.password}),
            None => json!({"response": "CancelAuth"}),
        };
        if c.is_none() {
            st.notes.push(format!("{origin} 要求 HTTP 登录（{}）：如果知道用户名和密码，用 browser_set credentials 设置后重新打开；否则请用户提供", p["authChallenge"]["scheme"].as_str().unwrap_or("Basic")));
        }
        drop(st);
        self.cdp.notify(Some(session), "Fetch.continueWithAuth", json!({"requestId": p["requestId"], "authChallengeResponse": answer}));
    }

    pub(crate) fn on_download(&self, method: &str, p: &Value) {
        let guid = p["guid"].as_str().unwrap_or("").to_string();
        let mut st = self.state.lock().unwrap();
        match method {
            "Browser.downloadWillBegin" => st.downloads.push(Download {
                guid,
                url: crate::clip(p["url"].as_str().unwrap_or(""), 500),
                name: sanitize(p["suggestedFilename"].as_str().unwrap_or("")),
                state: "inProgress".into(),
                path: None,
                claimed: false,
            }),
            "Browser.downloadProgress" => {
                let state = p["state"].as_str().unwrap_or("").to_string();
                let dir = self.download_dir.clone();
                if let Some(d) = st.downloads.iter_mut().find(|d| d.guid == guid) {
                    if state == "completed" && d.path.is_none() {
                        // allowAndName saves under the guid: give it the suggested name, like a browser would
                        if let Some(dir) = dir {
                            let to = unique_path(&dir, &d.name);
                            d.path = Some(if std::fs::rename(dir.join(&guid), &to).is_ok() { to } else { dir.join(&guid) });
                        }
                    }
                    d.state = state;
                }
            }
            _ => return,
        }
        drop(st);
        self.version.send_modify(|v| *v += 1);
    }

    /// Interception is on while there is an allowlist, a route, origin-scoped headers or credentials
    pub(crate) fn fetch_wanted(&self) -> bool {
        let st = self.state.lock().unwrap();
        !self.allowed.is_empty() || !st.routes.is_empty() || !st.scoped_headers.is_empty() || !st.credentials.is_empty()
    }

    pub(crate) async fn set_fetch(&self, session: &str, on: bool) -> Result<()> {
        if on {
            // Auth challenges come here too, so a page without credentials shows its 401 page and the model is told
            self.cdp.call(Some(session), "Fetch.enable", json!({"patterns": [{"urlPattern": "*", "requestStage": "Request"}], "handleAuthRequests": true})).await?;
        } else {
            self.cdp.call(Some(session), "Fetch.disable", json!({})).await?;
        }
        Ok(())
    }

    /// Turn interception on or off in every session after the routes changed
    pub(crate) async fn sync_fetch(&self) -> Result<()> {
        let on = self.fetch_wanted();
        let sessions: Vec<String> = {
            let st = self.state.lock().unwrap();
            st.tabs.iter().map(|t| t.session.clone()).chain(st.oopif.values().cloned()).chain(st.workers.iter().cloned()).collect()
        };
        for s in sessions {
            self.set_fetch(&s, on).await?;
        }
        Ok(())
    }
}

impl Browser {
    pub(crate) fn check_allowed(&self, url: &str) -> Result<()> {
        if !self.inner.allowed.is_empty() && !domain_allowed(&self.inner.allowed, url) {
            bail!("{} 不在这台主机设置的允许域名列表里", crate::tools::origin(url));
        }
        Ok(())
    }

    /// browser_network: requests (read) / route / unroute / clear
    pub async fn network(&self, a: &Value) -> Result<String> {
        let action = a["action"].as_str().unwrap_or("requests");
        match action {
            "requests" => {
                let filter = a["filter"].as_str().unwrap_or("").to_lowercase();
                let kind = a["type"].as_str().unwrap_or("").to_lowercase();
                let failed = a["failed"].as_bool().unwrap_or(false);
                let method = a["method"].as_str().unwrap_or("").to_ascii_uppercase();
                let code = a["code"].as_str().map(String::from).or_else(|| a["code"].as_i64().map(|n| n.to_string())).unwrap_or_default();
                let limit = a["limit"].as_u64().unwrap_or(50).clamp(1, REQUEST_LIMIT as u64) as usize;
                let tab = self.current()?;
                let st = self.state();
                let empty = Default::default();
                let list: Vec<&Request> = st
                    .requests
                    .get(&tab.target_id)
                    .unwrap_or(&empty)
                    .iter()
                    .filter(|r| filter.is_empty() || r.url.to_lowercase().contains(&filter))
                    .filter(|r| kind.is_empty() || r.kind.to_lowercase() == kind)
                    .filter(|r| !failed || r.failed.is_some() || r.status.is_some_and(|s| s >= 400))
                    .filter(|r| method.is_empty() || r.method == method)
                    .filter(|r| code.is_empty() || status_matches(&code, r.status))
                    .collect();
                let skip = list.len().saturating_sub(limit);
                if list.is_empty() {
                    return Ok("（没有匹配的请求）".into());
                }
                Ok(list[skip..]
                    .iter()
                    .map(|r| {
                        let status = match (&r.failed, r.status) {
                            (Some(f), _) => format!("失败 {f}"),
                            (None, Some(s)) => s.to_string(),
                            (None, None) => "进行中".into(),
                        };
                        let ms = r.ms.map(|m| format!(" {m:.0}ms")).unwrap_or_default();
                        format!("#{} {} {} {} [{}]{}", r.id, r.method, r.url, status, r.kind, ms)
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "detail" => {
                let id = a["id"].as_str().map(|s| s.trim().trim_start_matches('#').to_string()).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("detail 需要 id（requests 列表里 # 后面的编号）"))?;
                let tab = self.current()?;
                let r = self.state().requests.get(&tab.target_id).and_then(|l| l.iter().rev().find(|r| r.id == id).cloned()).ok_or_else(|| anyhow!("没有请求 #{id}（记录只保留每个标签页最近 {REQUEST_LIMIT} 个）"))?;
                let mut out = vec![format!("{} {}", r.method, r.url)];
                out.push(match (&r.failed, r.status) {
                    (Some(f), _) => format!("结果：失败 {f}"),
                    (None, Some(s)) => format!("状态：{s}{}", if r.mime.is_empty() { String::new() } else { format!("，{}", r.mime) }),
                    (None, None) => "状态：进行中".into(),
                });
                out.push("请求头：".into());
                out.extend(header_lines(&r.req_headers).into_iter().map(|l| format!("  {l}")));
                let post = match (&r.post_data, r.has_post) {
                    (Some(p), _) => Some(p.clone()),
                    (None, true) => self.cdp().call(Some(&r.session), "Network.getRequestPostData", json!({"requestId": r.id})).await.ok().and_then(|v| v["postData"].as_str().map(|s| crate::clip(s, 10_000))),
                    _ => None,
                };
                if let Some(p) = post {
                    out.push(format!("请求体：\n{p}"));
                }
                if !r.resp_headers.is_null() {
                    out.push("返回头：".into());
                    out.extend(header_lines(&r.resp_headers).into_iter().map(|l| format!("  {l}")));
                }
                if r.status.is_some() && r.failed.is_none() {
                    match self.cdp().call(Some(&r.session), "Network.getResponseBody", json!({"requestId": r.id})).await {
                        Ok(b) => {
                            let raw = b["body"].as_str().unwrap_or("");
                            let text = if b["base64Encoded"].as_bool() == Some(true) {
                                let bytes = base64::engine::general_purpose::STANDARD.decode(raw).unwrap_or_default();
                                match String::from_utf8(bytes.clone()) {
                                    Ok(t) if !r.mime.starts_with("image/") => t,
                                    _ => format!("（二进制内容，{} 字节）", bytes.len()),
                                }
                            } else {
                                raw.to_string()
                            };
                            out.push(format!("返回内容：\n{}", crate::clip(&text, 20_000)));
                        }
                        Err(_) => out.push("返回内容：读不到（重定向、被缓存清掉或页面已离开）".into()),
                    }
                }
                Ok(out.join("\n"))
            }
            "route" => {
                let pattern = a["url"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("route 需要 url 匹配模式"))?.to_string();
                let body = match &a["body"] {
                    Value::Null => String::new(),
                    Value::String(s) => s.clone(),
                    v => v.to_string(),
                };
                if body.len() > 1 << 20 {
                    bail!("返回内容过大");
                }
                let headers: Vec<(String, String)> = a["headers"]
                    .as_object()
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect())
                    .unwrap_or_default();
                if headers.iter().any(|(k, v)| k.is_empty() || k.contains(['\r', '\n', ':']) || v.contains(['\r', '\n'])) {
                    bail!("headers 里有无效的名称或值");
                }
                // A JSON body given as text is still JSON
                let looks_json = |b: &str| {
                    let t = b.trim_start();
                    (t.starts_with('{') || t.starts_with('[')) && serde_json::from_str::<Value>(b).is_ok()
                };
                let content_type = a["contentType"].as_str().map(String::from).unwrap_or_else(|| {
                    if !a["body"].is_string() || looks_json(&body) { "application/json".into() } else { "text/plain; charset=utf-8".into() }
                });
                let route = Route {
                    pattern: pattern.clone(),
                    abort: a["abort"].as_bool().unwrap_or(false),
                    status: a["status"].as_u64().unwrap_or(200).clamp(100, 599) as u16,
                    content_type,
                    body,
                    headers,
                    resource_type: a["resourceType"].as_str().filter(|s| !s.is_empty()).map(String::from),
                };
                {
                    let mut st = self.state();
                    st.routes.retain(|r| r.pattern != pattern);
                    if st.routes.len() >= 50 {
                        bail!("拦截规则太多（最多 50 条）");
                    }
                    st.routes.push(route.clone());
                }
                self.inner.sync_fetch().await?;
                Ok(format!("已添加拦截规则：{pattern} → {}", if route.abort { "中止请求".to_string() } else { format!("返回 {}", route.status) }))
            }
            "unroute" => {
                let n = {
                    let mut st = self.state();
                    let before = st.routes.len();
                    match a["url"].as_str().filter(|s| !s.is_empty()) {
                        Some(p) => st.routes.retain(|r| r.pattern != p),
                        None => st.routes.clear(),
                    }
                    before - st.routes.len()
                };
                self.inner.sync_fetch().await?;
                Ok(format!("已移除 {n} 条拦截规则"))
            }
            "clear" => {
                let tab = self.current()?;
                self.state().requests.remove(&tab.target_id);
                Ok("已清空请求记录".into())
            }
            other => bail!("未知的 action {other}"),
        }
    }

    /// Wait for the oldest finished download not reported yet (among those not in `skip`), and claim it, like
    /// agent-browser's download ledger: two downloads finishing close together each go to one call
    async fn next_download(&self, skip: &[String], timeout: Duration) -> Result<String> {
        if self.inner.download_dir.is_none() {
            bail!("这台主机不允许浏览器下载文件");
        }
        let pick = |st: &crate::browser::State| st.downloads.iter().position(|d| d.state != "inProgress" && !d.claimed && !skip.contains(&d.guid));
        self.wait_until(timeout, |st| pick(st).is_some()).await.map_err(|_| {
            let running = self.state().downloads.iter().filter(|d| d.state == "inProgress").count();
            if running > 0 { anyhow!("等待下载超时：还有 {running} 个下载没有完成") } else { anyhow!("等待下载超时：没有开始任何下载") }
        })?;
        let d = {
            let mut st = self.state();
            let i = pick(&st).ok_or_else(|| anyhow!("没有下载"))?;
            st.downloads[i].claimed = true;
            st.downloads[i].clone()
        };
        match (d.state.as_str(), &d.path) {
            ("completed", Some(p)) => {
                let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                Ok(format!("已下载 {}（{size} 字节）到 {}", d.name, p.display()))
            }
            ("canceled", _) => bail!("下载 {} 被取消了（页面取消、磁盘空间不足或文件被拦截）", d.name),
            _ => bail!("下载 {} 没有完成（{}）", d.name, d.state),
        }
    }

    /// browser_download: click a ref and wait for the file it starts
    pub async fn download(&self, r: &str, timeout_ms: u64) -> Result<String> {
        // Only a download that starts after the click is this click's
        let before: Vec<String> = self.state().downloads.iter().map(|d| d.guid.clone()).collect();
        let clicked = self.click(r, "click").await?;
        let saved = self.next_download(&before, Duration::from_millis(timeout_ms.clamp(1000, 300_000))).await?;
        Ok(format!("{clicked}\n{saved}"))
    }

    /// browser_wait download: the oldest finished download not reported yet (also one that finished before this
    /// call), else the next one to finish
    pub(crate) async fn wait_download(&self, timeout: Duration) -> Result<String> {
        self.next_download(&[], timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_matching() {
        let a = vec!["example.com".to_string(), "*.test.dev".to_string(), "127.0.0.1".into()];
        assert!(domain_allowed(&a, "https://example.com/x"));
        assert!(!domain_allowed(&a, "https://www.example.com/x"));
        assert!(!domain_allowed(&a, "https://example.com.evil.io/"));
        assert!(domain_allowed(&a, "https://test.dev"));
        assert!(domain_allowed(&a, "wss://a.b.test.dev:8443/socket"));
        assert!(!domain_allowed(&a, "https://nottest.dev"));
        assert!(domain_allowed(&a, "http://127.0.0.1:5173/"));
        assert!(!domain_allowed(&a, "http://user@evil.io/"));
        assert!(!domain_allowed(&a, "http://example.com@evil.io/"));
        assert!(domain_allowed(&a, "data:text/plain,x"));
        assert!(domain_allowed(&a, "blob:https://evil.io/1"));
    }

    #[test]
    fn download_names() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(sanitize("../a:b.txt"), "_a_b.txt");
        assert_eq!(sanitize(""), "download");
        assert_eq!(unique_path(dir.path(), "r.csv"), dir.path().join("r.csv"));
        std::fs::write(dir.path().join("r.csv"), b"").unwrap();
        assert_eq!(unique_path(dir.path(), "r.csv"), dir.path().join("r (1).csv"));
    }

    #[test]
    fn headers_status_and_cors() {
        let lines = header_lines(&json!({"Cookie": "a=1; b=2", "accept": "text/html", "Authorization": "Bearer xyz"}));
        assert_eq!(lines, ["accept: text/html", "Authorization: <已隐藏，10 个字符>", "Cookie: <已隐藏，8 个字符>"]);
        assert!(status_matches("404", Some(404)) && !status_matches("404", Some(400)));
        assert!(status_matches("4xx", Some(418)) && !status_matches("4xx", Some(500)));
        assert!(status_matches(">=400", Some(503)) && !status_matches(">=400", Some(302)));
        assert!(!status_matches("4xx", None));
        let r = Route { pattern: "*".into(), abort: false, status: 200, body: "{}".into(), content_type: "application/json".into(), headers: vec![("X-A".into(), "1".into())], resource_type: None };
        let h = mock_headers(&r, &json!({"Origin": "http://localhost:5173"}));
        assert!(h.contains(&("Access-Control-Allow-Origin".into(), "http://localhost:5173".into())));
        assert!(h.contains(&("Access-Control-Allow-Credentials".into(), "true".into())));
        assert!(h.contains(&("X-A".into(), "1".into())) && h.iter().any(|(k, _)| k == "Content-Type"));
        assert!(mock_headers(&r, &json!({})).contains(&("Access-Control-Allow-Origin".into(), "*".into())));
        let p = preflight_headers(&json!({"origin": "http://a.test", "Access-Control-Request-Method": "POST", "access-control-request-headers": "content-type,x-token"}));
        assert!(p.contains(&("Access-Control-Allow-Methods".into(), "POST".into())));
        assert!(p.contains(&("Access-Control-Allow-Headers".into(), "content-type,x-token".into())));
        assert!(blocked_page("http://e.vil/<x>").contains("&lt;x&gt;"));
    }

    #[test]
    fn guard_script_embeds_the_list() {
        let s = guard_script(&["a.test".into()]);
        assert!(s.contains(r#"const A=["a.test"]"#));
        assert!(s.contains("importScripts") && s.contains("sendBeacon"));
    }
}
