//! The live view people watch: a JPEG screencast of the current tab, the person's own input, and a summary of
//! where the browser is. Modeled on agent-browser's streaming (`Page.startScreencast`, every frame acked at once,
//! only the latest frame kept); transport and rate limits belong to the host.

use crate::browser::{Browser, Dialog, Inner};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::{Arc, Weak};

/// What the screencast should look like; None stops it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastConfig {
    pub max_width: u32,
    pub max_height: u32,
    /// JPEG quality, 1-100
    pub quality: u32,
}

/// One screencast frame. `metadata` is Chrome's (deviceWidth / deviceHeight are the viewport in CSS pixels)
#[derive(Debug)]
pub struct Frame {
    pub data: String,
    pub metadata: Value,
    pub tab: String,
}

/// The AI's mouse in viewport CSS pixels; `clicks` counts presses so a viewer can show each one
#[derive(Debug, Clone, PartialEq)]
pub struct Pointer {
    pub tab: String,
    pub x: f64,
    pub y: f64,
    pub clicks: u64,
}

/// Where the browser is, for the address and status bars
#[derive(Debug, Clone, PartialEq)]
pub struct ViewState {
    pub url: String,
    pub title: String,
    pub loading: bool,
    /// (id, url, title, current)
    pub tabs: Vec<(String, String, String, bool)>,
    pub dialog: Option<Dialog>,
    /// Label of the frame the AI switched into, if any
    pub frame: Option<String>,
    /// The tool the AI is running
    pub busy: Option<String>,
    /// The AI's mouse on the current tab
    pub pointer: Option<Pointer>,
}

impl PartialEq for Dialog {
    fn eq(&self, o: &Self) -> bool {
        self.kind == o.kind && self.message == o.message && self.session == o.session
    }
}

impl Inner {
    pub(crate) fn on_frame(&self, session: &str, p: &Value) {
        // Ack at once so Chrome keeps sending; slow viewers are the host's business (latest frame only)
        self.cdp.notify(Some(session), "Page.screencastFrameAck", json!({"sessionId": p["sessionId"]}));
        let tab = {
            let casting = self.casting.lock().unwrap();
            if casting.as_ref().map(|(s, _)| s.as_str()) != Some(session) {
                return;
            }
            self.state.lock().unwrap().session_tab.get(session).cloned().unwrap_or_default()
        };
        let data = p["data"].as_str().unwrap_or("").to_string();
        if data.is_empty() {
            return;
        }
        self.frames.send_replace(Some(Arc::new(Frame { data, metadata: p["metadata"].clone(), tab })));
    }
}

/// Keeps the screencast on the current tab while someone watches: restarts it when the tab changes
pub(crate) fn spawn_caster(inner: Weak<Inner>) {
    let Some(i) = inner.upgrade() else { return };
    let mut rx = i.version.subscribe();
    drop(i);
    tokio::spawn(async move {
        loop {
            let Some(i) = inner.upgrade() else { break };
            let want = {
                let cfg = i.cast.lock().unwrap().clone();
                let st = i.state.lock().unwrap();
                if st.disconnected {
                    break;
                }
                cfg.and_then(|c| st.current_tab().map(|t| (t.session.clone(), c)))
            };
            let have = i.casting.lock().unwrap().clone();
            if want != have {
                if let Some((s, _)) = &have {
                    let _ = i.cdp.call(Some(s), "Page.stopScreencast", json!({})).await;
                }
                *i.casting.lock().unwrap() = want.clone();
                if let Some((s, c)) = &want {
                    let r = i
                        .cdp
                        .call(
                            Some(s),
                            "Page.startScreencast",
                            json!({"format": "jpeg", "quality": c.quality.clamp(1, 100), "maxWidth": c.max_width, "maxHeight": c.max_height, "everyNthFrame": 1}),
                        )
                        .await;
                    if r.is_err() {
                        *i.casting.lock().unwrap() = None;
                    }
                }
            }
            drop(i);
            if rx.changed().await.is_err() {
                break;
            }
        }
    });
}

fn num(v: &Value, k: &str) -> f64 {
    v[k].as_f64().filter(|x| x.is_finite()).unwrap_or(0.0)
}

impl Browser {
    /// The latest screencast frame and every one after it
    pub fn frames(&self) -> tokio::sync::watch::Receiver<Option<Arc<Frame>>> {
        self.inner.frames.subscribe()
    }

    /// Start, change or stop (None) the screencast
    pub fn set_screencast(&self, cfg: Option<CastConfig>) {
        let changed = {
            let mut c = self.inner.cast.lock().unwrap();
            let changed = *c != cfg;
            *c = cfg;
            changed
        };
        if changed {
            self.inner.version.send_modify(|v| *v += 1);
        }
    }

    /// Bumped on every page event, tab change and call start/end: re-read `view_state` when it moves
    pub fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.version.subscribe()
    }

    pub fn view_state(&self) -> ViewState {
        let st = self.state();
        let cur = st.current_tab();
        ViewState {
            url: cur.map(|t| t.url.clone()).unwrap_or_default(),
            title: cur.map(|t| t.title.clone()).unwrap_or_default(),
            loading: cur.is_some_and(|t| !t.lifecycle.contains("load") && t.url != "about:blank"),
            tabs: st.tabs.iter().map(|t| (t.id.clone(), t.url.clone(), t.title.clone(), st.current.as_deref() == Some(t.target_id.as_str()))).collect(),
            dialog: st.dialog.clone(),
            frame: cur.and_then(|t| t.frame.as_ref().map(|f| f.label.clone())),
            busy: st.busy.clone(),
            pointer: st.pointer.clone().filter(|p| st.current.as_deref() == Some(p.tab.as_str())),
        }
    }

    /// A person's input on the live view, in viewport CSS pixels. Sent without waiting (a dialog would block the
    /// reply) and without the tool-call lock, so it never queues behind the AI. Refs it invalidates fail on the AI's
    /// next use, and the AI is told the page was touched
    pub fn human_input(&self, ev: &Value) -> Result<()> {
        let kind = ev["type"].as_str().unwrap_or("");
        if kind == "dialog" {
            let d = self.pending_dialog().ok_or_else(|| anyhow!("没有等待处理的对话框"))?;
            let mut p = json!({"accept": ev["accept"].as_bool().unwrap_or(false)});
            if let Some(t) = ev["text"].as_str() {
                p["promptText"] = json!(t);
            }
            self.cdp().notify(Some(&d.session), "Page.handleJavaScriptDialog", p);
            self.state().human = true;
            return Ok(());
        }
        let tab = self.current()?;
        let session = Some(tab.session.as_str());
        let modifiers = ev["modifiers"].as_u64().unwrap_or(0) & 0xf;
        match kind {
            "mouse" => {
                let t = match ev["event"].as_str().unwrap_or("") {
                    "down" => "mousePressed",
                    "up" => "mouseReleased",
                    "move" => "mouseMoved",
                    "wheel" => "mouseWheel",
                    e => bail!("未知的鼠标事件 {e}"),
                };
                let button = match ev["button"].as_str().unwrap_or("none") {
                    b @ ("left" | "middle" | "right" | "back" | "forward") => b,
                    _ => "none",
                };
                let mut p = json!({
                    "type": t, "x": num(ev, "x"), "y": num(ev, "y"), "modifiers": modifiers,
                    "button": if t == "mouseMoved" || t == "mouseWheel" { "none" } else { button },
                    "buttons": ev["buttons"].as_u64().unwrap_or(0) & 0x1f,
                    "clickCount": ev["clickCount"].as_u64().unwrap_or(if t == "mousePressed" || t == "mouseReleased" { 1 } else { 0 }).min(3),
                });
                if t == "mouseWheel" {
                    p["deltaX"] = json!(num(ev, "deltaX").clamp(-10_000.0, 10_000.0));
                    p["deltaY"] = json!(num(ev, "deltaY").clamp(-10_000.0, 10_000.0));
                }
                self.cdp().notify(session, "Input.dispatchMouseEvent", p);
            }
            "key" => {
                let text = ev["text"].as_str().unwrap_or("");
                if text.chars().count() > 4 {
                    bail!("按键文字过长");
                }
                let up = ev["event"] == "up";
                let t = if up { "keyUp" } else if text.is_empty() { "rawKeyDown" } else { "keyDown" };
                let code = ev["keyCode"].as_u64().unwrap_or(0).min(255);
                let mut p = json!({
                    "type": t, "modifiers": modifiers,
                    "key": ev["key"].as_str().unwrap_or(""), "code": ev["code"].as_str().unwrap_or(""),
                    "windowsVirtualKeyCode": code, "nativeVirtualKeyCode": code,
                    "autoRepeat": ev["repeat"].as_bool().unwrap_or(false),
                });
                if !up && !text.is_empty() {
                    p["text"] = json!(text);
                    p["unmodifiedText"] = json!(text);
                }
                self.cdp().notify(session, "Input.dispatchKeyEvent", p);
            }
            // Committed IME text, pastes and mobile keyboards
            "text" => {
                let text = ev["text"].as_str().unwrap_or("");
                if text.chars().count() > 10_000 {
                    bail!("输入的文字过长");
                }
                if !text.is_empty() {
                    self.cdp().notify(session, "Input.insertText", json!({"text": text}));
                }
            }
            "touch" => {
                let t = match ev["event"].as_str().unwrap_or("") {
                    "start" => "touchStart",
                    "move" => "touchMove",
                    "end" => "touchEnd",
                    "cancel" => "touchCancel",
                    e => bail!("未知的触摸事件 {e}"),
                };
                let points: Vec<Value> = ev["points"].as_array().into_iter().flatten().take(5).map(|p| json!({"x": num(p, "x"), "y": num(p, "y")})).collect();
                self.cdp().notify(session, "Input.dispatchTouchEvent", json!({"type": t, "touchPoints": points, "modifiers": modifiers}));
            }
            k => bail!("未知的输入类型 {k}"),
        }
        self.state().human = true;
        Ok(())
    }
}
