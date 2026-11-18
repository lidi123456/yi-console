//! browser_frame, browser_diff, browser_upload, browser_set, browser_eval.

use crate::browser::{Browser, Emulation, FrameSel};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

/// The document page-level reads and evals go to: the selected frame, else the tab's main frame
pub(crate) struct Doc {
    pub session: String,
    pub context: Option<i64>,
}

impl Browser {
    pub(crate) fn doc(&self) -> Result<Doc> {
        let tab = self.current()?;
        let st = self.state();
        match &tab.frame {
            Some(f) => {
                let context = st.contexts.get(&(f.session.clone(), f.frame.clone())).copied();
                if context.is_none() && !f.own {
                    bail!("所选框架 {} 已不存在，请用 browser_frame 回到主文档", f.label);
                }
                Ok(Doc { session: f.session.clone(), context })
            }
            None => Ok(Doc { session: tab.session.clone(), context: None }),
        }
    }

    /// Evaluate in the selected document
    pub(crate) async fn eval_doc(&self, expr: &str) -> Result<Value> {
        let d = self.doc()?;
        let mut p = json!({"expression": expr, "returnByValue": true, "awaitPromise": true});
        if let Some(c) = d.context {
            p["contextId"] = json!(c);
        }
        let r = self.cdp().call(Some(&d.session), "Runtime.evaluate", p).await?;
        if let Some(e) = r.get("exceptionDetails") {
            bail!("页面脚本出错：{}", e["exception"]["description"].as_str().or(e["text"].as_str()).unwrap_or(""));
        }
        Ok(r["result"]["value"].clone())
    }

    /// browser_frame: into the iframe of a ref, or back to the main document
    pub async fn frame(&self, r: Option<&str>) -> Result<String> {
        self.no_dialog()?;
        let tab = self.current()?;
        let Some(r) = r else {
            if let Some(t) = self.state().tabs.iter_mut().find(|t| t.target_id == tab.target_id) {
                t.frame = None;
            }
            self.inner.version.send_modify(|v| *v += 1);
            return Ok("已回到主文档".into());
        };
        let r = r.trim().trim_start_matches('@');
        let entry = self.state().refs.map.get(r).cloned().ok_or_else(|| anyhow!("没有 ref {r}，请先 browser_snapshot"))?;
        if entry.role != "Iframe" {
            bail!("{r} 不是 iframe（是 {}）", entry.role);
        }
        if !entry.valid || entry.tab != tab.target_id {
            bail!("{r} 已失效，请重新 browser_snapshot");
        }
        let (session, frame, own) = self.child_frame(&entry.session, entry.backend).await.ok_or_else(|| anyhow!("读不到 {r} 里的文档"))?;
        let label = if entry.name.is_empty() { r.to_string() } else { format!("{r} \"{}\"", crate::clip(&entry.name, 60)) };
        if let Some(t) = self.state().tabs.iter_mut().find(|t| t.target_id == tab.target_id) {
            t.frame = Some(FrameSel { session, frame, own, label: label.clone() });
        }
        self.inner.version.send_modify(|v| *v += 1);
        let url = self.eval_doc("location.href").await.ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
        Ok(format!("已进入框架 {label}：{url}\nbrowser_snapshot、browser_get、browser_eval 现在作用于这个框架；不带 ref 调用 browser_frame 回到主文档"))
    }

    /// browser_upload: set files on an `<input type=file>`. Paths are checked by the host
    pub async fn upload(&self, r: &str, files: &[PathBuf]) -> Result<String> {
        self.no_dialog()?;
        if files.is_empty() {
            bail!("缺少要上传的文件");
        }
        for f in files {
            if !f.is_file() {
                bail!("文件不存在：{}", f.display());
            }
        }
        let t = self.resolve(r).await?;
        let kind = self.call_on(&t.session, &t.object, "function(){return this.tagName==='INPUT'&&this.type==='file'?(this.multiple?'multi':'one'):'no'}", vec![]).await?;
        match kind.as_str() {
            Some("no") | None => bail!("{} 不是文件选择框（input type=file）", t.r),
            Some("one") if files.len() > 1 => bail!("{} 只能选一个文件", t.r),
            _ => {}
        }
        let list: Vec<String> = files.iter().map(|f| f.to_string_lossy().into_owned()).collect();
        self.cdp().call(Some(&t.session), "DOM.setFileInputFiles", json!({"files": list, "objectId": t.object})).await?;
        Ok(format!("已在 {} 选择 {} 个文件", t.r, files.len()))
    }

    /// browser_set: viewport and device, media features, network, locale, timezone, geolocation, permissions,
    /// headers and HTTP credentials. Kept for tabs opened later
    pub async fn set(&self, a: &Value) -> Result<String> {
        let mut e = self.state().emulation.clone();
        let mut done = vec![];
        let had_ua = e.user_agent.is_some();
        let reset = a["reset"].as_bool().unwrap_or(false);
        if reset {
            e = Emulation { reset: true, ..Default::default() };
            {
                let mut st = self.state();
                st.scoped_headers.clear();
                st.credentials.clear();
            }
            let _ = self.cdp().call(None, "Browser.resetPermissions", json!({})).await;
            self.inner.sync_fetch().await?;
            done.push("已恢复默认设置（视口、设备、媒体、网络、语言、时区、定位、权限、请求头、登录信息）".to_string());
        }
        if let Some(d) = a["device"].as_str().filter(|d| !d.is_empty()) {
            let (w, h, scale, ua) = device(d).ok_or_else(|| anyhow!("未知的设备 {d}，可用：{}", DEVICES.iter().map(|d| d.0).collect::<Vec<_>>().join("、")))?;
            e.viewport = Some((w, h));
            e.scale = Some(scale);
            let phone = !ua.is_empty();
            e.mobile = phone;
            e.touch = phone;
            e.user_agent = if phone { Some(ua.to_string()) } else { None };
            done.push(format!("设备 {d}（{w}×{h}，缩放 {scale}；已打开的页面重新加载后才完全按这个设备显示）"));
        }
        match (a["width"].as_u64(), a["height"].as_u64()) {
            (Some(w), Some(h)) => {
                if !(200..=3840).contains(&w) || !(200..=2160).contains(&h) {
                    bail!("视口宽度要在 200 到 3840 之间，高度在 200 到 2160 之间");
                }
                e.viewport = Some((w as u32, h as u32));
                done.push(format!("视口 {w}×{h}"));
            }
            (None, None) => {}
            _ => bail!("width 和 height 要一起设置"),
        }
        if let Some(f) = a["scale"].as_f64() {
            if !(0.5..=4.0).contains(&f) {
                bail!("scale 要在 0.5 到 4 之间");
            }
            e.scale = Some(f);
            done.push(format!("缩放 {f}"));
        }
        if let Some(m) = a["mobile"].as_bool() {
            e.mobile = m;
            e.touch = m;
            done.push(if m { "手机布局和触摸".into() } else { "桌面布局".into() });
        }
        if (e.scale.is_some() || e.mobile) && e.viewport.is_none() {
            e.viewport = Some(crate::launch::DEFAULT_VIEWPORT);
        }
        if let Some(u) = a["userAgent"].as_str() {
            e.user_agent = Some(u.to_string()).filter(|u| !u.is_empty());
            done.push("User-Agent".into());
        }
        if let Some(c) = a["colorScheme"].as_str() {
            if !matches!(c, "light" | "dark" | "no-preference") {
                bail!("colorScheme 只能是 light、dark 或 no-preference");
            }
            e.color = Some(c.to_string());
            done.push(format!("颜色方案 {c}"));
        }
        if let Some(m) = a["reducedMotion"].as_str() {
            if !matches!(m, "reduce" | "no-preference") {
                bail!("reducedMotion 只能是 reduce 或 no-preference");
            }
            e.reduced_motion = Some(m.to_string());
            done.push(format!("减少动画 {m}"));
        }
        if let Some(o) = a["offline"].as_bool() {
            e.offline = o;
            done.push(if o { "离线".to_string() } else { "在线".to_string() });
        }
        match &a["throttle"] {
            Value::Null => {}
            Value::String(p) => {
                e.throttle = match p.as_str() {
                    "slow3g" => Some((400.0, 400.0, 400.0)),
                    "fast3g" => Some((150.0, 1600.0, 750.0)),
                    "4g" => Some((40.0, 9000.0, 9000.0)),
                    "off" | "none" => None,
                    other => bail!("throttle 预设只能是 slow3g、fast3g、4g 或 off，不能是 {other}"),
                };
                done.push(format!("网络限速 {p}"));
            }
            v => {
                let n = |k: &str| v[k].as_f64();
                e.throttle = Some((n("latency").unwrap_or(0.0).clamp(0.0, 10_000.0), n("downloadKbps").unwrap_or(-1.0), n("uploadKbps").unwrap_or(-1.0)));
                done.push("网络限速".into());
            }
        }
        if let Some(l) = a["locale"].as_str() {
            if !l.is_empty() && !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                bail!("locale 格式应该像 zh-CN、en-US");
            }
            e.locale = Some(l.to_string()).filter(|l| !l.is_empty());
            done.push(format!("语言 {l}"));
        }
        if let Some(t) = a["timezone"].as_str() {
            e.timezone = Some(t.to_string()).filter(|t| !t.is_empty());
            done.push(format!("时区 {t}"));
        }
        if let Some(g) = a["geolocation"].as_object() {
            let lat = g.get("latitude").and_then(Value::as_f64).ok_or_else(|| anyhow!("geolocation 需要 latitude"))?;
            let lon = g.get("longitude").and_then(Value::as_f64).ok_or_else(|| anyhow!("geolocation 需要 longitude"))?;
            if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
                bail!("经纬度超出范围");
            }
            e.geolocation = Some((lat, lon, g.get("accuracy").and_then(Value::as_f64).unwrap_or(50.0)));
            // Reading it needs the permission too
            let _ = self.cdp().call(None, "Browser.grantPermissions", json!({"permissions": ["geolocation"]})).await;
            done.push(format!("定位 {lat},{lon}"));
        }
        if let Some(list) = a["permissions"].as_array() {
            let p: Vec<&str> = list.iter().filter_map(Value::as_str).collect();
            let r = self.cdp().call(None, "Browser.grantPermissions", json!({"permissions": p})).await;
            r.map_err(|e| anyhow!("授予权限失败（名称如 geolocation、notifications、clipboardReadWrite、camera、microphone）：{e}"))?;
            done.push(format!("权限 {}", p.join("、")));
        }
        if let Some(h) = a["headers"].as_object() {
            let headers: Vec<(String, String)> = h.iter().map(|(k, v)| (k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect();
            if headers.iter().any(|(k, v)| k.is_empty() || k.contains(['\r', '\n', ':']) || v.contains(['\r', '\n'])) {
                bail!("headers 里有无效的名称或值");
            }
            match a["headersOrigin"].as_str().filter(|o| !o.is_empty()) {
                Some(o) => {
                    let origin = crate::tools::origin(o.trim_end_matches('/'));
                    {
                        let mut st = self.state();
                        st.scoped_headers.retain(|s| !s.origin.eq_ignore_ascii_case(&origin));
                        if !headers.is_empty() {
                            st.scoped_headers.push(crate::net::ScopedHeaders { origin: origin.clone(), headers: headers.clone() });
                        }
                    }
                    self.inner.sync_fetch().await?;
                    done.push(format!("只发往 {origin} 的请求头 {}", headers.iter().map(|h| h.0.as_str()).collect::<Vec<_>>().join("、")));
                }
                None => {
                    e.headers = headers.clone();
                    done.push(format!("所有请求的请求头 {}", headers.iter().map(|h| h.0.as_str()).collect::<Vec<_>>().join("、")));
                }
            }
        }
        if let Some(c) = a["credentials"].as_object() {
            let user = c.get("username").and_then(Value::as_str).unwrap_or("").to_string();
            let origin = c.get("origin").and_then(Value::as_str).filter(|o| !o.is_empty()).map(|o| crate::tools::origin(o.trim_end_matches('/')));
            {
                let mut st = self.state();
                st.credentials.retain(|x| x.origin != origin);
                if !user.is_empty() {
                    st.credentials.push(crate::net::Credentials { origin: origin.clone(), username: user, password: c.get("password").and_then(Value::as_str).unwrap_or("").to_string() });
                }
            }
            self.inner.sync_fetch().await?;
            done.push(format!("HTTP 登录信息（{}）", origin.as_deref().unwrap_or("所有网站")));
        }
        if done.is_empty() {
            bail!("browser_set 没有要设置的内容");
        }
        if had_ua && e.user_agent.is_none() {
            e.reset_ua = true;
        }
        self.state().emulation = e.clone();
        let tabs: Vec<String> = self.state().tabs.iter().map(|t| t.session.clone()).collect();
        for s in tabs {
            self.apply_emulation(&s, &e).await?;
        }
        {
            let mut st = self.state();
            st.emulation.reset = false;
            st.emulation.reset_ua = false;
        }
        Ok(format!("已设置：{}", done.join("，")))
    }

    /// browser_cookies: get / set / delete / clear (httpOnly ones too, which page scripts can't see)
    pub async fn cookies(&self, a: &Value) -> Result<(String, bool)> {
        self.no_dialog()?;
        let tab = self.current()?;
        let url = a["url"].as_str().filter(|u| !u.is_empty()).map(String::from).unwrap_or_else(|| tab.url.clone());
        let name = a["name"].as_str().unwrap_or("");
        match a["action"].as_str().unwrap_or("get") {
            "get" => {
                let r = if a["all"].as_bool() == Some(true) {
                    self.cdp().call(None, "Storage.getCookies", json!({})).await?
                } else {
                    self.cdp().call(Some(&tab.session), "Network.getCookies", json!({"urls": [url]})).await?
                };
                let list: Vec<String> = r["cookies"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|c| name.is_empty() || c["name"] == name)
                    .map(|c| {
                        let mut f = vec![format!("{}={}", c["name"].as_str().unwrap_or(""), crate::clip(c["value"].as_str().unwrap_or(""), 300))];
                        f.push(format!("domain={}", c["domain"].as_str().unwrap_or("")));
                        f.push(format!("path={}", c["path"].as_str().unwrap_or("/")));
                        for (k, l) in [("httpOnly", "httpOnly"), ("secure", "secure")] {
                            if c[k] == json!(true) {
                                f.push(l.into());
                            }
                        }
                        if let Some(s) = c["sameSite"].as_str() {
                            f.push(format!("sameSite={s}"));
                        }
                        match c["expires"].as_f64() {
                            Some(t) if t > 0.0 => f.push(format!("expires={}", t as i64)),
                            _ => f.push("session".into()),
                        }
                        f.join("; ")
                    })
                    .collect();
                Ok((if list.is_empty() { "（没有 Cookie）".into() } else { list.join("\n") }, true))
            }
            "set" => {
                if name.is_empty() {
                    bail!("set 需要 name");
                }
                let mut p = json!({"name": name, "value": a["value"].as_str().unwrap_or("")});
                match a["domain"].as_str().filter(|d| !d.is_empty()) {
                    Some(d) => {
                        p["domain"] = json!(d);
                        p["path"] = json!(a["path"].as_str().unwrap_or("/"));
                    }
                    None => {
                        if !url.starts_with("http") {
                            bail!("当前页面不是网页：用 url 或 domain 指定 Cookie 属于哪个网站");
                        }
                        p["url"] = json!(url);
                        if let Some(path) = a["path"].as_str() {
                            p["path"] = json!(path);
                        }
                    }
                }
                for k in ["httpOnly", "secure"] {
                    if let Some(b) = a[k].as_bool() {
                        p[k] = json!(b);
                    }
                }
                if let Some(s) = a["sameSite"].as_str() {
                    p["sameSite"] = json!(s);
                }
                if let Some(t) = a["expires"].as_f64() {
                    p["expires"] = json!(t);
                }
                let r = self.cdp().call(Some(&tab.session), "Network.setCookie", p).await?;
                if r["success"] == json!(false) {
                    bail!("浏览器拒绝了这个 Cookie（检查 domain、secure 和 sameSite=None 需要 secure）");
                }
                Ok((format!("已设置 Cookie {name}"), false))
            }
            "delete" => {
                if name.is_empty() {
                    bail!("delete 需要 name");
                }
                let mut p = json!({"name": name});
                match a["domain"].as_str().filter(|d| !d.is_empty()) {
                    Some(d) => p["domain"] = json!(d),
                    None => p["url"] = json!(url),
                }
                if let Some(path) = a["path"].as_str() {
                    p["path"] = json!(path);
                }
                self.cdp().call(Some(&tab.session), "Network.deleteCookies", p).await?;
                Ok((format!("已删除 Cookie {name}"), false))
            }
            "clear" => {
                self.cdp().call(Some(&tab.session), "Network.clearBrowserCookies", json!({})).await?;
                Ok(("已清除这个浏览器的所有 Cookie".into(), false))
            }
            other => bail!("未知的 action {other}"),
        }
    }

    /// browser_storage: localStorage / sessionStorage of the selected document
    pub async fn storage(&self, a: &Value) -> Result<(String, bool)> {
        self.no_dialog()?;
        let area = match a["kind"].as_str().unwrap_or("local") {
            "local" => "localStorage",
            "session" => "sessionStorage",
            other => bail!("kind 只能是 local 或 session，不能是 {other}"),
        };
        let key = serde_json::to_string(a["key"].as_str().unwrap_or(""))?;
        let value = serde_json::to_string(a["value"].as_str().unwrap_or(""))?;
        let action = a["action"].as_str().unwrap_or("get");
        let expr = match action {
            "get" if a["key"].as_str().is_some_and(|k| !k.is_empty()) => format!("{area}.getItem({key})"),
            "get" => format!("Object.fromEntries(Object.keys({area}).slice(0,200).map(k=>[k,String({area}.getItem(k)).slice(0,500)]))"),
            "set" if a["key"].as_str().is_some_and(|k| !k.is_empty()) => format!("({area}.setItem({key},{value}),true)"),
            "remove" if a["key"].as_str().is_some_and(|k| !k.is_empty()) => format!("({area}.removeItem({key}),true)"),
            "clear" => format!("({area}.clear(),true)"),
            "set" | "remove" => bail!("{action} 需要 key"),
            other => bail!("未知的 action {other}"),
        };
        let v = self.eval_doc(&expr).await.map_err(|e| anyhow!("读写 {area} 失败（about:blank 和部分沙箱页面没有存储）：{e}"))?;
        Ok(match action {
            "get" => (
                match v {
                    Value::Null => "（没有这个键）".into(),
                    Value::String(s) => s,
                    Value::Object(m) if m.is_empty() => format!("（{area} 是空的）"),
                    v => serde_json::to_string_pretty(&v)?,
                },
                true,
            ),
            "set" => (format!("已写入 {area} {}", a["key"].as_str().unwrap_or("")), false),
            "remove" => (format!("已删除 {area} {}", a["key"].as_str().unwrap_or("")), false),
            _ => (format!("已清空 {area}"), false),
        })
    }

    pub(crate) async fn apply_emulation(&self, session: &str, e: &Emulation) -> Result<()> {
        apply_emulation(self.cdp(), session, e).await
    }

    /// browser_eval: an expression (or async function body result) in the selected document, as JSON
    pub async fn eval_tool(&self, expr: &str) -> Result<String> {
        self.no_dialog()?;
        if expr.len() > 100_000 {
            bail!("脚本过长");
        }
        let v = self.eval_doc(expr).await?;
        Ok(match v {
            Value::Null => "null".into(),
            v => serde_json::to_string_pretty(&v)?,
        })
    }
}

/// Device presets for browser_set device: (name, width, height, scale, user agent; empty = desktop)
const DEVICES: &[(&str, u32, u32, f64, &str)] = &[
    ("iPhone 15", 393, 852, 3.0, "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1"),
    ("iPhone SE", 375, 667, 2.0, "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1"),
    ("Pixel 8", 412, 915, 2.625, "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Mobile Safari/537.36"),
    ("Galaxy S23", 360, 780, 3.0, "Mozilla/5.0 (Linux; Android 14; SM-S911B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Mobile Safari/537.36"),
    ("iPad", 820, 1180, 2.0, "Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1"),
    ("Desktop", 1280, 800, 1.0, ""),
    ("Desktop HD", 1920, 1080, 1.0, ""),
];

pub fn device(name: &str) -> Option<(u32, u32, f64, &'static str)> {
    let n = name.trim().to_ascii_lowercase().replace(['_', '-'], " ");
    DEVICES.iter().find(|d| d.0.to_ascii_lowercase() == n).map(|d| (d.1, d.2, d.3, d.4))
}

pub(crate) async fn apply_emulation(cdp: &crate::cdp::Cdp, session: &str, e: &Emulation) -> Result<()> {
    let s = Some(session);
    match e.viewport {
        Some((w, h)) => {
            cdp.call(s, "Emulation.setDeviceMetricsOverride", json!({"width": w, "height": h, "deviceScaleFactor": e.scale.unwrap_or(1.0), "mobile": e.mobile})).await?;
        }
        None if e.reset => {
            cdp.call(s, "Emulation.clearDeviceMetricsOverride", json!({})).await?;
        }
        None => {}
    }
    if e.touch || e.reset {
        let _ = cdp.call(s, "Emulation.setTouchEmulationEnabled", json!({"enabled": e.touch, "maxTouchPoints": if e.touch { 5 } else { 1 }})).await;
    }
    match &e.user_agent {
        Some(ua) => {
            cdp.call(s, "Emulation.setUserAgentOverride", json!({"userAgent": ua})).await?;
        }
        None if e.reset || e.reset_ua => {
            // There is no "clear": set the browser's own one back
            let v = cdp.call(None, "Browser.getVersion", json!({})).await?;
            let ua = v["userAgent"].as_str().unwrap_or("").replace("HeadlessChrome", "Chrome");
            cdp.call(s, "Emulation.setUserAgentOverride", json!({"userAgent": ua})).await?;
        }
        None => {}
    }
    let mut media = vec![];
    if let Some(c) = &e.color {
        media.push(json!({"name": "prefers-color-scheme", "value": c}));
    }
    if let Some(m) = &e.reduced_motion {
        media.push(json!({"name": "prefers-reduced-motion", "value": m}));
    }
    if !media.is_empty() || e.reset {
        cdp.call(s, "Emulation.setEmulatedMedia", json!({"features": media})).await?;
    }
    if e.offline || e.throttle.is_some() || e.reset {
        let (lat, down, up) = e.throttle.unwrap_or((0.0, -1.0, -1.0));
        let bytes = |kbps: f64| if kbps < 0.0 { -1.0 } else { kbps * 1000.0 / 8.0 };
        cdp.call(s, "Network.emulateNetworkConditions", json!({"offline": e.offline, "latency": lat, "downloadThroughput": bytes(down), "uploadThroughput": bytes(up)})).await?;
    }
    if e.locale.is_some() || e.reset {
        // Fails when another override is active for the same target; harmless
        let _ = cdp.call(s, "Emulation.setLocaleOverride", match &e.locale { Some(l) => json!({"locale": l}), None => json!({}) }).await;
    }
    if let Some(t) = &e.timezone {
        cdp.call(s, "Emulation.setTimezoneOverride", json!({"timezoneId": t})).await.map_err(|_| anyhow!("无效的时区 {t}（应该像 Asia/Shanghai、America/New_York）"))?;
    } else if e.reset {
        let _ = cdp.call(s, "Emulation.setTimezoneOverride", json!({"timezoneId": ""})).await;
    }
    match e.geolocation {
        Some((lat, lon, acc)) => {
            cdp.call(s, "Emulation.setGeolocationOverride", json!({"latitude": lat, "longitude": lon, "accuracy": acc})).await?;
        }
        None if e.reset => {
            let _ = cdp.call(s, "Emulation.clearGeolocationOverride", json!({})).await;
        }
        None => {}
    }
    if !e.headers.is_empty() || e.reset {
        let h: serde_json::Map<String, Value> = e.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        cdp.call(s, "Network.setExtraHTTPHeaders", json!({"headers": h})).await?;
    }
    Ok(())
}

/// Line diff of two snapshots: `+` added, `-` removed, unchanged lines left out. LCS on lines; very large pairs
/// fall back to set difference
pub fn diff_lines(old: &str, new: &str) -> (String, usize, usize) {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut out = vec![];
    let (mut add, mut del) = (0, 0);
    if a.len().saturating_mul(b.len()) > 4_000_000 {
        let sa: std::collections::HashSet<&str> = a.iter().copied().collect();
        let sb: std::collections::HashSet<&str> = b.iter().copied().collect();
        for l in &a {
            if !sb.contains(l) {
                out.push(format!("- {l}"));
                del += 1;
            }
        }
        for l in &b {
            if !sa.contains(l) {
                out.push(format!("+ {l}"));
                add += 1;
            }
        }
        return (out.join("\n"), add, del);
    }
    let (n, m) = (a.len(), b.len());
    let mut t = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[at(i, j)] = if a[i] == b[j] { t[at(i + 1, j + 1)] + 1 } else { t[at(i + 1, j)].max(t[at(i, j + 1)]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j < m && (i == n || t[at(i, j + 1)] >= t[at(i + 1, j)]) {
            out.push(format!("+ {}", b[j]));
            add += 1;
            j += 1;
        } else {
            out.push(format!("- {}", a[i]));
            del += 1;
            i += 1;
        }
    }
    (out.join("\n"), add, del)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices() {
        assert_eq!(device("iphone-15").map(|d| (d.0, d.1)), Some((393, 852)));
        assert_eq!(device("Desktop").map(|d| d.3), Some(""));
        assert!(device("nokia").is_none());
    }

    #[test]
    fn snapshot_diff() {
        let (d, add, del) = diff_lines("- a\n- b\n- c", "- a\n- c\n- d");
        assert_eq!(d, "- - b\n+ - d");
        assert_eq!((add, del), (1, 1));
        assert_eq!(diff_lines("x", "x"), (String::new(), 0, 0));
    }
}
