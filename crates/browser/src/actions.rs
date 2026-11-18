//! Actions on refs and pages. A click checks first that its target is visible, enabled, in the viewport and not
//! covered, and fails instead of reporting success otherwise (agent-browser's rule). Coordinates come from the
//! element's own frame and are shifted by the frame offsets, as Puppeteer's `clickablePoint` does.

use crate::browser::{Browser, Tab};
use crate::keys;
use anyhow::{anyhow, bail, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::time::Duration;

/// Where to send input for one element
pub(crate) struct Target {
    pub session: String,
    pub object: String,
    pub r: String,
}

const CHECK_JS: &str = r#"function(mode){
const el0=this;
if(!el0.isConnected) return {err:'detached'};
const describe=n=>{ if(!n||!n.tagName) return 'unknown'; let s=n.tagName.toLowerCase(); if(n.id) s+='#'+n.id; const c=(typeof n.className==='string'?n.className:'').trim().split(/\s+/).filter(Boolean).slice(0,3); if(c.length) s+='.'+c.join('.'); const t=(n.innerText||'').replace(/\s+/g,' ').trim().slice(0,60); if(t) s+=' "'+t+'"'; return s; };
const hidden=n=>{ const st=getComputedStyle(n); const r=n.getBoundingClientRect(); return (r.width===0&&r.height===0)||st.visibility==='hidden'||st.display==='none'||parseFloat(st.opacity)===0; };
let el=el0;
// A visually hidden checkbox or radio is usually operated through its label
if(hidden(el)&&el.labels&&el.labels.length){ const l=[...el.labels].find(l=>!hidden(l)); if(l) el=l; }
if(hidden(el)) return {err:'hidden'};
if(el0.disabled||el0.getAttribute('aria-disabled')==='true'||(el0.closest&&el0.closest('fieldset[disabled]'))) return {err:'disabled'};
const r=el.getBoundingClientRect();
const x=r.left+Math.min(r.width/2, Math.max(1, r.width-1)), y=r.top+Math.min(r.height/2, Math.max(1, r.height-1));
if(x<0||y<0||x>innerWidth||y>innerHeight) return {err:'offscreen'};
let hit=document.elementFromPoint(x,y);
while(hit&&hit.shadowRoot){ const h=hit.shadowRoot.elementFromPoint(x,y); if(!h||h===hit) break; hit=h; }
const inside=n=>{ for(let c=n;c;c=c.parentNode||c.host){ if(c===el||c===el0) return true; } return false; };
// The hit is an ancestor of the target (the target has pointer-events:none, or is text inside a clickable box): a
// person clicking there reaches the same handler, so it isn't covered (agent-browser's BLOCKER_AT_JS)
const holds=n=>{ for(let c=el0;c;c=c.parentNode||c.host){ if(c===n) return true; } return false; };
const viaLabel=hit&&el0.labels&&[...el0.labels].some(l=>l.contains(hit));
if(mode!=='hover'&&hit&&!inside(hit)&&!holds(hit)&&!viaLabel) return {err:'covered', by:describe(hit)};
let ox=0, oy=0;
try{ let w=window; while(w.frameElement){ const f=w.frameElement, b=f.getBoundingClientRect(), cs=w.parent.getComputedStyle(f); ox+=b.left+parseFloat(cs.borderLeftWidth)+parseFloat(cs.paddingLeft); oy+=b.top+parseFloat(cs.borderTopWidth)+parseFloat(cs.paddingTop); w=w.parent; } }catch(e){}
return {x:x+ox, y:y+oy};
}"#;

/// `{el, __n}`: the first visible match of a CSS selector or XPath, and how many matched; null when none
const SELECTOR_JS: &str = r#"function(kind, sel){
let list;
if(kind==='xpath'){ const r=document.evaluate(sel, document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null); list=[]; for(let i=0;i<r.snapshotLength;i++){ const n=r.snapshotItem(i); if(n.nodeType===1) list.push(n); } }
else list=[...document.querySelectorAll(sel)];
if(!list.length) return null;
const vis=e=>{ const b=e.getBoundingClientRect(); return b.width>0&&b.height>0; };
return {el:list.find(vis)||list[0], __n:list.length};
}"#;

/// Elements matching a locator (agent-browser's `find`): `{els, info}` by reference, visible ones first
pub(crate) const FIND_JS: &str = r#"function(by, v, exact){
const norm=s=>(s||'').replace(/[\s\u00a0]+/g,' ').trim();
const want=norm(v).toLowerCase();
const hit=s=>{ const t=norm(s).toLowerCase(); return exact?t===want:t.includes(want); };
const all=[...document.querySelectorAll('*')];
let els=[];
switch(by){
 case 'text': els=all.filter(e=>!['SCRIPT','STYLE','HEAD','HTML','BODY','NOSCRIPT'].includes(e.tagName)&&hit(e.innerText)&&![...e.children].some(c=>hit(c.innerText))); break;
 case 'label': els=all.filter(e=>{ if(e.labels&&[...e.labels].some(l=>hit(l.innerText))) return true; if(e.getAttribute('aria-label')&&hit(e.getAttribute('aria-label'))) return true; const ids=(e.getAttribute('aria-labelledby')||'').split(/\s+/).filter(Boolean); return ids.length&&hit(ids.map(i=>{const n=document.getElementById(i);return n?n.innerText:''}).join(' ')); }); break;
 case 'placeholder': els=all.filter(e=>e.hasAttribute('placeholder')&&hit(e.getAttribute('placeholder'))); break;
 case 'alt': els=all.filter(e=>e.hasAttribute('alt')&&hit(e.getAttribute('alt'))); break;
 case 'title': els=all.filter(e=>e.hasAttribute('title')&&hit(e.getAttribute('title'))); break;
 case 'testid': els=all.filter(e=>['data-testid','data-test-id','data-test','data-cy','data-qa'].some(a=>e.getAttribute(a)===v)); break;
 case 'css': els=[...document.querySelectorAll(v)]; break;
 default: throw new Error('unknown locator '+by);
}
const vis=e=>{ const b=e.getBoundingClientRect(), s=getComputedStyle(e); return b.width>0&&b.height>0&&s.visibility!=='hidden'; };
els=[...els.filter(vis), ...els.filter(e=>!vis(e))].slice(0,30);
const info=els.map(e=>({tag:e.tagName.toLowerCase(), text:norm(e.innerText||e.value||e.getAttribute('aria-label')||e.getAttribute('placeholder')||e.getAttribute('alt')||'').slice(0,80), visible:vis(e)}));
return {els, info};
}"#;

/// Options of an open custom listbox anywhere in the document (they often render in a portal): `{els, info}`
const OPTIONS_JS: &str = r#"(()=>{
const norm=s=>(s||'').replace(/[\s\u00a0]+/g,' ').trim();
const vis=e=>{ const b=e.getBoundingClientRect(), s=getComputedStyle(e); return b.width>0&&b.height>0&&s.visibility!=='hidden'&&s.display!=='none'; };
const els=[...document.querySelectorAll('[role=option]')].filter(vis).slice(0,200);
const info=els.map(e=>({text:norm(e.innerText||e.textContent), label:norm(e.getAttribute('aria-label')), value:e.getAttribute('data-value')||'', selected:e.getAttribute('aria-selected')==='true', disabled:e.getAttribute('aria-disabled')==='true'}));
return {els, info};
})()"#;

/// Scroll inside an element: itself when it scrolls, else its nearest scrolling ancestor
const SCROLL_EL_JS: &str = r#"function(dx, dy){
const can=e=>{ const s=getComputedStyle(e); return /(auto|scroll|overlay)/.test(s.overflowY+s.overflowX)&&(e.scrollHeight>e.clientHeight+1||e.scrollWidth>e.clientWidth+1); };
let el=this; while(el&&el!==document.body&&el!==document.documentElement&&!can(el)) el=el.parentElement;
if(!el||el===document.body||el===document.documentElement) return {page:true};
el.scrollBy(dx, dy);
return {top:Math.round(el.scrollTop), left:Math.round(el.scrollLeft), height:el.scrollHeight, client:el.clientHeight, tag:el.tagName.toLowerCase()+(el.id?'#'+el.id:'')};
}"#;

/// Resolves once the element's box is the same over two animation frames (at most ~5 rounds), so a click doesn't land
/// mid-transition on a drawer or modal; an element still off screen gets one centered scroll
const SETTLE_JS: &str = r#"async function(){
const el=this; const raf=()=>new Promise(r=>requestAnimationFrame(()=>requestAnimationFrame(r)));
const tick=()=>Promise.race([raf(), new Promise(r=>setTimeout(r,100))]);
let last=null;
for(let i=0;i<5;i++){
  const b=el.getBoundingClientRect(); const k=[b.left,b.top,b.width,b.height].map(Math.round).join();
  if(k===last) break; last=k; await tick();
}
const b=el.getBoundingClientRect();
if(b.bottom<0||b.top>innerHeight||b.right<0||b.left>innerWidth){ el.scrollIntoView({block:'center',inline:'center'}); await tick(); }
return true;
}"#;

/// Offset of an OOPIF's content inside its tab: the `<iframe>` element's content box in the parent frame
const OWNER_JS: &str = r#"function(){
const b=this.getBoundingClientRect(), cs=getComputedStyle(this);
let ox=b.left+parseFloat(cs.borderLeftWidth)+parseFloat(cs.paddingLeft), oy=b.top+parseFloat(cs.borderTopWidth)+parseFloat(cs.paddingTop);
try{ let w=window; while(w.frameElement){ const f=w.frameElement.getBoundingClientRect(); ox+=f.left; oy+=f.top; w=w.parent; } }catch(e){}
return {x:ox, y:oy};
}"#;

const FILL_PREP_JS: &str = r#"function(){
const el=this;
if(el.disabled) return 'disabled';
if(el.readOnly) return 'readonly';
const editable=el.isContentEditable||el.tagName==='TEXTAREA'||(el.tagName==='INPUT'&&!['checkbox','radio','button','submit','reset','file','image','range','color'].includes(el.type));
if(!editable) return 'noteditable';
el.focus();
if(el.isContentEditable){ const r=document.createRange(); r.selectNodeContents(el); const s=getSelection(); s.removeAllRanges(); s.addRange(r); }
else if(typeof el.select==='function'){ el.select(); }
return 'ok';
}"#;

const SELECT_JS: &str = r#"function(values){
const el=this;
if(el.tagName!=='SELECT') return {err:'notselect', role:el.getAttribute('role')||'', expanded:el.getAttribute('aria-expanded')};
if(el.disabled) return {err:'disabled'};
// Visible labels match with ordinary and non-breaking spaces treated alike
const norm=s=>(s||'').replace(/[\s\u00a0]+/g,' ').trim();
const opts=[...el.options]; const picked=[];
for(const v of values){ const o=opts.find(o=>o.value===v)||opts.find(o=>norm(o.label)===norm(v)||norm(o.text)===norm(v)); if(!o) return {err:'nooption', value:v, options:opts.slice(0,50).map(o=>norm(o.label)||o.value)}; picked.push(o); }
if(!el.multiple&&picked.length>1) return {err:'single'};
for(const o of opts) o.selected=picked.includes(o);
el.dispatchEvent(new Event('input',{bubbles:true})); el.dispatchEvent(new Event('change',{bubbles:true}));
return {ok:picked.map(o=>o.label||o.value)};
}"#;

const CHECKED_JS: &str = r#"function(){
let el=this;
// A label standing for a hidden radio / checkbox (snapshot promotes those)
if(el.tagName==='LABEL'&&el.control) el=el.control;
else if(!('checked' in el)&&el.querySelector){ const i=el.querySelector('input[type=radio],input[type=checkbox]'); if(i) el=i; }
if(el.type==='checkbox'||el.type==='radio') return el.checked;
const a=el.getAttribute('aria-checked')||el.getAttribute('aria-pressed')||el.getAttribute('aria-selected');
if(a==='true') return true; if(a==='false') return false;
return null;
}"#;

const GET_JS: &str = r#"function(kind, arg){
const el=this;
switch(kind){
 case 'attr': return el.getAttribute(arg);
 case 'box': { const r=el.getBoundingClientRect(); return {x:Math.round(r.left), y:Math.round(r.top), width:Math.round(r.width), height:Math.round(r.height), pageX:Math.round(r.left+scrollX), pageY:Math.round(r.top+scrollY)}; }
 case 'styles': { const cs=getComputedStyle(el); const ks=(arg||'display,position,color,background-color,font-family,font-size,font-weight,line-height,margin,padding,border,width,height,opacity,z-index,overflow').split(',').map(s=>s.trim()).filter(Boolean); const o={}; for(const k of ks) o[k]=cs.getPropertyValue(k); return o; }
 case 'text': return el.innerText!==undefined?el.innerText:el.textContent;
 case 'html': return el.outerHTML;
 case 'value': return el.value!==undefined?String(el.value):(el.getAttribute('aria-valuenow')||el.textContent);
 case 'visible': { const r=el.getBoundingClientRect(), s=getComputedStyle(el); return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none'; }
 case 'enabled': return !(el.disabled||el.getAttribute('aria-disabled')==='true');
 case 'checked': return el.checked!==undefined?el.checked:el.getAttribute('aria-checked')==='true';
}
return null;
}"#;

fn check_error(r: &str, v: &Value) -> anyhow::Error {
    match v["err"].as_str().unwrap_or("") {
        "detached" => anyhow!("{r} 对应的元素已从页面移除，请重新 browser_snapshot"),
        "hidden" => anyhow!("{r} 当前不可见，无法操作"),
        "disabled" => anyhow!("{r} 处于禁用状态"),
        "offscreen" => anyhow!("{r} 不在视口内，滚动后仍然看不到"),
        "covered" => anyhow!("{r} 被另一个元素挡住了：{}。先关闭或处理挡住它的元素（例如 Cookie 横幅、弹层），重新 browser_snapshot 后再试", v["by"].as_str().unwrap_or("")),
        e => anyhow!("{r} 无法操作：{e}"),
    }
}

impl Browser {
    /// A ref from the latest snapshots, resolved to a live object. A replaced node is found again by role and name
    /// only when they were unique in its snapshot
    pub(crate) async fn resolve(&self, r: &str) -> Result<Target> {
        let r = r.trim().trim_start_matches('@');
        if r.starts_with("css=") || r.starts_with("xpath=") {
            return self.resolve_selector(r).await;
        }
        let tab = self.current()?;
        let entry = self.state().refs.map.get(r).cloned().ok_or_else(|| anyhow!("没有 ref {r}，请先 browser_snapshot 并使用其中的 ref"))?;
        if entry.tab != tab.target_id {
            bail!("{r} 属于另一个标签页，请切换过去或在当前标签页重新 browser_snapshot");
        }
        if !entry.valid {
            bail!("{r} 已失效（页面或所在框架已导航），请重新 browser_snapshot");
        }
        match self.object_for(&entry.session, entry.backend).await {
            Ok(object) => Ok(Target { session: entry.session, object, r: r.into() }),
            Err(_) if entry.unique => {
                let own = entry.session != tab.session;
                let t = self.frame_tree(&entry.session, &entry.frame, own || entry.frame == tab.main_frame).await?;
                let found: Vec<i64> = t.nodes.values().filter(|n| !n.ignored && n.role == entry.role && n.name == entry.name).filter_map(|n| n.backend).collect();
                if found.len() != 1 {
                    bail!("{r} 对应的元素已被替换，且无法唯一确定新元素，请重新 browser_snapshot");
                }
                let object = self.object_for(&entry.session, found[0]).await?;
                let mut st = self.state();
                st.refs.by_node.insert((entry.session.clone(), found[0]), r.to_string());
                if let Some(e) = st.refs.map.get_mut(r) {
                    e.backend = found[0];
                }
                Ok(Target { session: entry.session, object, r: r.into() })
            }
            Err(_) => bail!("{r} 对应的元素已被替换，请重新 browser_snapshot"),
        }
    }

    /// `css=<selector>` / `xpath=<expression>` in the selected document: the first visible match (else the first)
    async fn resolve_selector(&self, r: &str) -> Result<Target> {
        self.current()?;
        let (kind, sel) = r.split_once('=').unwrap_or(("css", r));
        let doc = self.doc()?;
        let expr = format!("({SELECTOR_JS})({},{})", serde_json::to_string(kind)?, serde_json::to_string(sel)?);
        let mut q = json!({"expression": expr});
        if let Some(c) = doc.context {
            q["contextId"] = json!(c);
        }
        let v = self.cdp().call(Some(&doc.session), "Runtime.evaluate", q).await?;
        if let Some(e) = v.get("exceptionDetails") {
            let msg = e["exception"]["description"].as_str().unwrap_or("");
            bail!("选择器无效：{}", crate::clip(msg.lines().next().unwrap_or(msg), 200));
        }
        let obj = v["result"]["objectId"].as_str().ok_or_else(|| anyhow!("没有元素匹配{}，先 browser_snapshot 或 browser_find 看看页面", target_label(r)))?.to_string();
        let n = self.call_on(&doc.session, &obj, "function(){return this.__n}", vec![]).await.ok().and_then(|v| v.as_u64()).unwrap_or(1);
        let el = self.cdp().call(Some(&doc.session), "Runtime.callFunctionOn", json!({"objectId": obj, "functionDeclaration": "function(){return this.el}"})).await?;
        let object = el["result"]["objectId"].as_str().ok_or_else(|| anyhow!("没有元素匹配{}", target_label(r)))?.to_string();
        let label = if n > 1 { format!("{}（{n} 个匹配，用了第一个可见的）", target_label(r)) } else { target_label(r) };
        Ok(Target { session: doc.session, object, r: label })
    }

    /// Viewport point (in tab coordinates) to act on, after the checks
    pub(crate) async fn point(&self, t: &Target, mode: &str) -> Result<(f64, f64)> {
        let _ = self.cdp().call(Some(&t.session), "DOM.scrollIntoViewIfNeeded", json!({"objectId": t.object})).await;
        // Wait for animations and smooth scrolling to put the element where it stays (agent-browser's CLICK_POINT_JS)
        let _ = self.call_on(&t.session, &t.object, SETTLE_JS, vec![]).await;
        let v = self.call_on(&t.session, &t.object, CHECK_JS, vec![json!(mode)]).await?;
        if v.get("err").is_some() {
            return Err(check_error(&t.r, &v));
        }
        let (mut x, mut y) = (v["x"].as_f64().unwrap_or(0.0), v["y"].as_f64().unwrap_or(0.0));
        let tab = self.current()?;
        if t.session != tab.session {
            // An out-of-process iframe: add where its <iframe> element sits in the tab
            let frame = self.state().oopif.iter().find(|(_, s)| **s == t.session).map(|(f, _)| f.clone());
            if let Some(fid) = frame {
                let owner = self.cdp().call(Some(&tab.session), "DOM.getFrameOwner", json!({"frameId": fid})).await?;
                if let Some(b) = owner["backendNodeId"].as_i64() {
                    let obj = self.object_for(&tab.session, b).await?;
                    let o = self.call_on(&tab.session, &obj, OWNER_JS, vec![]).await?;
                    x += o["x"].as_f64().unwrap_or(0.0);
                    y += o["y"].as_f64().unwrap_or(0.0);
                }
            }
        }
        Ok((x, y))
    }

    /// Send an input command; a confirm/prompt opened by it blocks the command until handled, so return as soon as
    /// one shows up
    pub(crate) async fn input(&self, tab: &Tab, method: &str, params: Value) -> Result<bool> {
        let rx = self.cdp().send(Some(&tab.session), method, params)?;
        let opened = self.wait_until(Duration::from_secs(20), |st| st.dialog.is_some());
        tokio::select! {
            r = rx => match r {
                Ok(Ok(_)) => Ok(false),
                Ok(Err(e)) => bail!("{method}: {e}"),
                Err(_) => bail!("浏览器连接已断开"),
            },
            r = opened => match r {
                Ok(()) => Ok(true),
                Err(_) => bail!("浏览器响应超时（{method}）"),
            },
        }
    }

    /// One mouse event. `held`: buttons held during a move (drags)
    #[allow(clippy::too_many_arguments)]
    async fn mouse_with(&self, tab: &Tab, kind: &str, x: f64, y: f64, button: &str, count: u32, modifiers: i64, held: Option<&str>) -> Result<bool> {
        let mask = |b: &str| match b {
            "right" => 2,
            "middle" => 4,
            "left" => 1,
            _ => 0,
        };
        let mut p = json!({"type": kind, "x": x, "y": y, "button": button, "clickCount": count, "modifiers": modifiers});
        if kind == "mouseMoved" {
            p["button"] = json!(held.unwrap_or("none"));
            p["buttons"] = json!(held.map(mask).unwrap_or(0));
        } else {
            p["buttons"] = json!(if kind == "mousePressed" { mask(button) } else { 0 });
        }
        {
            let mut st = self.state();
            let clicks = st.pointer.as_ref().map(|p| p.clicks).unwrap_or(0) + u64::from(kind == "mousePressed");
            st.pointer = Some(crate::view::Pointer { tab: tab.target_id.clone(), x, y, clicks });
        }
        self.inner.bump();
        self.input(tab, "Input.dispatchMouseEvent", p).await
    }

    /// click / dblclick / rightclick / middleclick / hover / focus on a ref or selector
    pub async fn click(&self, r: &str, kind: &str) -> Result<String> {
        self.click_with(r, kind, &[]).await
    }

    /// `modifiers`: Alt, Control, Meta, Shift held during the click (multi-select, open in new tab)
    pub async fn click_with(&self, r: &str, kind: &str, modifiers: &[String]) -> Result<String> {
        self.no_dialog()?;
        let t = self.resolve(r).await?;
        self.click_target(&t, kind, modifier_mask(modifiers)?).await
    }

    pub(crate) async fn click_target(&self, t: &Target, kind: &str, modifiers: i64) -> Result<String> {
        let tab = self.current()?;
        if kind == "focus" {
            self.cdp().call(Some(&t.session), "DOM.focus", json!({"objectId": t.object})).await.map_err(|_| anyhow!("{} 不能获得焦点", t.r))?;
            return Ok(format!("已聚焦 {}", t.r));
        }
        let (x, y) = self.point(t, if kind == "hover" { "hover" } else { "click" }).await?;
        let before = self.mark();
        if self.mouse_with(&tab, "mouseMoved", x, y, "none", 0, modifiers, None).await? {
            return Ok(self.dialog_text());
        }
        if kind != "hover" {
            let button = match kind {
                "rightclick" => "right",
                "middleclick" => "middle",
                _ => "left",
            };
            let rounds = if kind == "dblclick" { 2 } else { 1 };
            for n in 1..=rounds {
                if self.mouse_with(&tab, "mousePressed", x, y, button, n, modifiers, None).await? || self.mouse_with(&tab, "mouseReleased", x, y, button, n, modifiers, None).await? {
                    return Ok(self.dialog_text());
                }
            }
        }
        let verb = match kind {
            "dblclick" => "双击了",
            "rightclick" => "右键点击了",
            "middleclick" => "中键点击了",
            "hover" => "悬停在",
            _ => "点击了",
        };
        Ok(format!("{verb} {}{}", t.r, self.after_action(&before).await))
    }

    /// Drag one element onto another: mouse down, moves, up. An HTML5 drag (draggable=true) is intercepted and
    /// finished with drag events, which plain mouse events don't produce in an automated Chrome
    pub async fn drag(&self, from: &str, to: &str) -> Result<String> {
        self.no_dialog()?;
        let tab = self.current()?;
        let a = self.resolve(from).await?;
        let (x1, y1) = self.point(&a, "click").await?;
        let b = self.resolve(to).await?;
        let (x2, y2) = self.point(&b, "hover").await?;
        let before = self.mark();
        self.state().drag_data = None;
        let _ = self.cdp().call(Some(&tab.session), "Input.setInterceptDrags", json!({"enabled": true})).await;
        let r = async {
            self.mouse_with(&tab, "mouseMoved", x1, y1, "none", 0, 0, None).await?;
            self.mouse_with(&tab, "mousePressed", x1, y1, "left", 1, 0, None).await?;
            let steps = 10;
            for i in 1..=steps {
                let f = i as f64 / steps as f64;
                self.mouse_with(&tab, "mouseMoved", x1 + (x2 - x1) * f, y1 + (y2 - y1) * f, "none", 0, 0, Some("left")).await?;
                tokio::time::sleep(Duration::from_millis(16)).await;
            }
            let data = self.state().drag_data.take();
            if let Some(data) = data {
                for kind in ["dragEnter", "dragOver", "drop"] {
                    self.cdp().call(Some(&tab.session), "Input.dispatchDragEvent", json!({"type": kind, "x": x2, "y": y2, "data": data, "modifiers": 0})).await?;
                }
            }
            self.mouse_with(&tab, "mouseReleased", x2, y2, "left", 1, 0, None).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let _ = self.cdp().call(Some(&tab.session), "Input.setInterceptDrags", json!({"enabled": false})).await;
        r?;
        Ok(format!("已把 {} 拖到 {}{}", a.r, b.r, self.after_action(&before).await))
    }

    /// What changed right after an action: a navigation means the old refs are gone
    async fn after_action(&self, before: &Mark) -> String {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if let Some(d) = self.pending_dialog() {
            return format!("\n{}", dialog_line(&d));
        }
        let now = self.mark();
        if now.navs != before.navs {
            let _ = self.wait_load("load", Duration::from_secs(10)).await;
            format!("\n页面已跳转到 {}，原来的 ref 已失效，请重新 browser_snapshot", self.current_url())
        } else if now.url != before.url {
            format!("\n页面地址变为 {}（同一页面内）", now.url)
        } else if now.tab != before.tab {
            format!("\n当前标签页变为 {}", now.url)
        } else {
            String::new()
        }
    }

    /// Where the current tab is, to tell what an action changed
    fn mark(&self) -> Mark {
        let st = self.state();
        match st.current_tab() {
            Some(t) => Mark { tab: t.target_id.clone(), url: t.url.clone(), navs: t.navs },
            None => Mark::default(),
        }
    }

    fn dialog_text(&self) -> String {
        self.pending_dialog().map(|d| dialog_line(&d)).unwrap_or_default()
    }

    /// Everything but browser_dialog waits behind an open confirm/prompt: the page is frozen until it is answered
    pub(crate) fn no_dialog(&self) -> Result<()> {
        match self.pending_dialog() {
            Some(d) => bail!("{}", dialog_line(&d)),
            None => Ok(()),
        }
    }

    pub async fn fill(&self, r: &str, text: &str, submit: bool) -> Result<String> {
        self.no_dialog()?;
        let t = self.resolve(r).await?;
        let tab = self.current()?;
        match self.call_on(&t.session, &t.object, FILL_PREP_JS, vec![]).await?.as_str().unwrap_or("") {
            "ok" => {}
            "disabled" => bail!("{} 处于禁用状态", t.r),
            "readonly" => bail!("{} 是只读的", t.r),
            _ => bail!("{} 不是可输入的文本框", t.r),
        }
        if text.is_empty() {
            keys::press(self, &tab, "Delete").await?;
        } else {
            self.cdp().call(Some(&tab.session), "Input.insertText", json!({"text": text})).await?;
        }
        let before = self.mark();
        if submit && keys::press(self, &tab, "Enter").await? {
            return Ok(self.dialog_text());
        }
        Ok(format!("已在 {} 填入 {} 个字符{}{}", t.r, text.chars().count(), if submit { "并按了回车" } else { "" }, self.after_action(&before).await))
    }

    pub async fn type_text(&self, r: Option<&str>, text: &str) -> Result<String> {
        self.no_dialog()?;
        let tab = self.current()?;
        if let Some(r) = r {
            let t = self.resolve(r).await?;
            self.cdp().call(Some(&t.session), "DOM.focus", json!({"objectId": t.object})).await?;
        }
        for ch in text.chars() {
            if keys::type_char(self, &tab, ch).await? {
                return Ok(self.dialog_text());
            }
        }
        Ok(format!("已输入 {} 个字符", text.chars().count()))
    }

    pub async fn select(&self, r: &str, values: &[String]) -> Result<String> {
        self.no_dialog()?;
        let t = self.resolve(r).await?;
        let v = self.call_on(&t.session, &t.object, SELECT_JS, vec![json!(values)]).await?;
        match v["err"].as_str() {
            None => Ok(format!("已在 {} 选择 {}", t.r, v["ok"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("、")).unwrap_or_default())),
            Some("notselect") => self.select_custom(&t, values, v["expanded"].as_str() == Some("true")).await,
            Some("disabled") => bail!("{} 处于禁用状态", t.r),
            Some("single") => bail!("{} 只能选一项", t.r),
            Some(_) => bail!("{} 没有选项 {}，可选：{}", t.r, v["value"].as_str().unwrap_or(""), v["options"]),
        }
    }

    /// A combobox / listbox that isn't a <select> (Radix, Headless UI, MUI ...): open it, wait for its options, click
    /// the matching ones (agent-browser's `select` on custom widgets)
    async fn select_custom(&self, t: &Target, values: &[String], open: bool) -> Result<String> {
        if !open {
            self.click_target(t, "click", 0).await?;
        }
        let doc = self.doc()?;
        let norm = |s: &str| s.replace('\u{a0}', " ").split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let opts = loop {
            let o = self.collect_elements(&doc.session, doc.context, OPTIONS_JS).await.unwrap_or_default();
            if !o.is_empty() || tokio::time::Instant::now() >= deadline {
                break o;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        };
        if opts.is_empty() {
            bail!("{} 不是 <select>，点开后也没有出现选项（role=option）：先 browser_click 打开它，再 browser_snapshot 找到选项点击", t.r);
        }
        let mut picked = vec![];
        for v in values {
            let w = norm(v);
            let hits: Vec<&(String, Value)> = opts.iter().filter(|(_, i)| [i["text"].as_str(), i["label"].as_str(), i["value"].as_str()].iter().flatten().any(|s| norm(s) == w)).collect();
            let hit = match hits.as_slice() {
                [one] => *one,
                [] => {
                    let names: Vec<&str> = opts.iter().filter_map(|(_, i)| i["text"].as_str()).take(30).collect();
                    bail!("{} 没有选项 {v}，可选：{}", t.r, names.join("、"));
                }
                _ => bail!("{} 有 {} 个选项都叫 {v}，请 browser_snapshot 后直接点击要的那个", t.r, hits.len()),
            };
            if hit.1["disabled"].as_bool() == Some(true) {
                bail!("选项 {v} 处于禁用状态");
            }
            // Re-clicking an already selected option of a multi-select would unselect it
            if hit.1["selected"].as_bool() == Some(true) && values.len() > 1 {
                picked.push(v.clone());
                continue;
            }
            let o = Target { session: doc.session.clone(), object: hit.0.clone(), r: format!("选项 {v}") };
            self.click_target(&o, "click", 0).await?;
            picked.push(v.clone());
        }
        Ok(format!("已在 {} 选择 {}", t.r, picked.join("、")))
    }

    pub async fn check(&self, r: &str, checked: bool) -> Result<String> {
        self.no_dialog()?;
        let t = self.resolve(r).await?;
        let now = self.call_on(&t.session, &t.object, CHECKED_JS, vec![]).await?;
        if now.as_bool() == Some(checked) {
            return Ok(format!("{} 已经是{}状态", t.r, if checked { "勾选" } else { "未勾选" }));
        }
        let msg = self.click(r, "click").await?;
        let after = self.resolve(r).await.ok();
        let state = match after {
            Some(t) => self.call_on(&t.session, &t.object, CHECKED_JS, vec![]).await.ok().and_then(|v| v.as_bool()),
            None => None,
        };
        match state {
            Some(s) if s != checked => bail!("点击了 {} 但状态没有变化", t.r),
            _ => Ok(msg),
        }
    }

    pub async fn press(&self, combo: &str) -> Result<String> {
        self.no_dialog()?;
        let tab = self.current()?;
        let before = self.mark();
        if keys::press(self, &tab, combo).await? {
            return Ok(self.dialog_text());
        }
        Ok(format!("已按下 {combo}{}", self.after_action(&before).await))
    }

    /// Without a ref: the page. A ref alone: scroll it into view. A ref and a direction: scroll inside it (a chat
    /// pane, a table body, a code panel with its own scrollbar)
    pub async fn scroll(&self, r: Option<&str>, direction: Option<&str>, amount: f64) -> Result<String> {
        self.no_dialog()?;
        let tab = self.current()?;
        let (dx, dy) = match direction.unwrap_or("down") {
            "up" => (0.0, -amount),
            "left" => (-amount, 0.0),
            "right" => (amount, 0.0),
            _ => (0.0, amount),
        };
        if let Some(r) = r {
            let t = self.resolve(r).await?;
            if direction.is_none() {
                self.cdp().call(Some(&t.session), "DOM.scrollIntoViewIfNeeded", json!({"objectId": t.object})).await?;
                return Ok(format!("已把 {} 滚动到可见位置", t.r));
            }
            let v = self.call_on(&t.session, &t.object, SCROLL_EL_JS, vec![json!(dx), json!(dy)]).await?;
            if v["page"] != json!(true) {
                tokio::time::sleep(Duration::from_millis(100)).await;
                return Ok(format!("已在 {}（{}）里滚动，当前位置 {}，内容高度 {}，可见高度 {}", t.r, v["tag"].as_str().unwrap_or(""), v["top"], v["height"], v["client"]));
            }
            // Nothing scrolls around it: scroll the page instead
        }
        let m = self.eval(&tab.session, "[innerWidth/2, innerHeight/2]").await?;
        let (x, y) = (m[0].as_f64().unwrap_or(100.0), m[1].as_f64().unwrap_or(100.0));
        self.input(&tab, "Input.dispatchMouseEvent", json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy})).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let pos = self.eval(&tab.session, "[Math.round(scrollX), Math.round(scrollY), Math.round(document.documentElement.scrollHeight)]").await.unwrap_or(Value::Null);
        Ok(format!("已滚动，当前位置 x={} y={}，页面高度 {}", pos[0], pos[1], pos[2]))
    }

    /// Wait for a lifecycle state of the current tab's main frame: load / domcontentloaded / networkidle
    pub(crate) async fn wait_load(&self, state: &str, timeout: Duration) -> Result<()> {
        let name = match state {
            "domcontentloaded" => "DOMContentLoaded",
            "networkidle" => "networkIdle",
            _ => "load",
        };
        let target = self.current()?.target_id;
        self.wait_until(timeout, |st| st.dialog.is_some() || st.tab(&target).is_none_or(|t| t.lifecycle.contains(name))).await
    }

    pub async fn open(&self, url: &str) -> Result<String> {
        self.no_dialog()?;
        let u = check_url(url)?;
        self.check_allowed(&u)?;
        // A crashed page can still be navigated away from
        let tab = self.current_raw()?;
        self.state().tab_gone = None;
        let r = self.cdp().call(Some(&tab.session), "Page.navigate", json!({"url": u})).await?;
        if let Some(e) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
            bail!("无法打开 {u}：{e}{}", nav_hint(e, &u));
        }
        self.settle().await
    }

    /// back / forward / reload
    pub async fn history(&self, action: &str) -> Result<String> {
        self.no_dialog()?;
        let tab = if action == "reload" { self.current_raw()? } else { self.current()? };
        if action == "reload" {
            self.cdp().call(Some(&tab.session), "Page.reload", json!({})).await?;
            return self.settle().await;
        }
        let h = self.cdp().call(Some(&tab.session), "Page.getNavigationHistory", json!({})).await?;
        let i = h["currentIndex"].as_i64().unwrap_or(0) + if action == "back" { -1 } else { 1 };
        let entry = h["entries"].as_array().and_then(|e| usize::try_from(i).ok().and_then(|i| e.get(i))).ok_or_else(|| anyhow!("没有可以{}的页面", if action == "back" { "后退" } else { "前进" }))?;
        self.cdp().call(Some(&tab.session), "Page.navigateToHistoryEntry", json!({"entryId": entry["id"]})).await?;
        self.settle().await
    }

    /// After a navigation: wait for load (but not forever), then say where we are
    async fn settle(&self) -> Result<String> {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let loaded = self.wait_load("load", Duration::from_secs(30)).await.is_ok();
        if let Some(d) = self.pending_dialog() {
            return Ok(dialog_line(&d));
        }
        let tab = self.current()?;
        let title = self.eval(&tab.session, "document.title").await.ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
        let mut s = format!("{}：{}\n标题：{}", if loaded { "已打开" } else { "仍在加载" }, tab.url, crate::clip(&title, 200));
        // The page loaded, but maybe as an error page of the server, or with scripts failing
        let (status, errors) = self.state().tab(&tab.target_id).map(|t| (t.doc_status, t.errors)).unwrap_or_default();
        if let Some(code) = status.filter(|c| *c >= 400) {
            s.push_str(&format!("\n服务器返回 HTTP {code}，这是一个错误页面"));
        }
        if errors > 0 {
            s.push_str(&format!("\n页面加载后有 {errors} 条控制台错误，用 browser_console level=error 查看"));
        }
        Ok(s)
    }

    /// list / new / select / close
    pub async fn tabs(&self, action: &str, id: Option<&str>, url: Option<&str>) -> Result<String> {
        match action {
            "new" => {
                let u = match url {
                    Some(u) if !u.is_empty() => {
                        let u = check_url(u)?;
                        self.check_allowed(&u)?;
                        u
                    }
                    _ => "about:blank".into(),
                };
                let r = self.cdp().call(None, "Target.createTarget", json!({"url": "about:blank"})).await?;
                let target = r["targetId"].as_str().unwrap_or("").to_string();
                self.wait_until(Duration::from_secs(10), |st| st.tab(&target).is_some()).await?;
                {
                    let mut st = self.state();
                    st.current = Some(target.clone());
                    st.tab_gone = None;
                    // Opened by the model itself: no "new tab" note
                    st.notes.retain(|n| !n.contains("新标签页"));
                }
                self.inner.bump();
                if u != "about:blank" {
                    self.open(&u).await?;
                }
            }
            "select" | "close" => {
                let id = id.ok_or_else(|| anyhow!("缺少标签页 id"))?;
                let target = self.state().tabs.iter().find(|t| t.id == id).map(|t| t.target_id.clone()).ok_or_else(|| anyhow!("没有标签页 {id}"))?;
                if action == "select" {
                    {
                        let mut st = self.state();
                        st.current = Some(target.clone());
                        st.tab_gone = None;
                    }
                    self.inner.bump();
                    let _ = self.cdp().call(None, "Target.activateTarget", json!({"targetId": target})).await;
                    if let Some(note) = self.revive(&target).await {
                        return Ok(format!("{}\n{note}", self.tab_text()));
                    }
                } else {
                    if self.state().tabs.len() == 1 {
                        bail!("这是最后一个标签页，不能关闭");
                    }
                    self.cdp().call(None, "Target.closeTarget", json!({"targetId": target})).await?;
                    let _ = self.wait_until(Duration::from_secs(5), |st| st.tab(&target).is_none()).await;
                    // Closed by the AI itself: it knows
                    self.state().tab_gone = None;
                }
            }
            "list" => {}
            _ => bail!("未知的标签页操作 {action}"),
        }
        Ok(self.tab_text())
    }

    /// A tab that doesn't answer (discarded by Chrome, renderer hung or crashed) is reloaded once, and the model is
    /// told the page was reloaded (agent-browser's `ensure_renderer_alive`)
    async fn revive(&self, target: &str) -> Option<String> {
        let (session, crashed, dialog) = {
            let st = self.state();
            let t = st.tab(target)?;
            (t.session.clone(), t.crashed, st.dialog.as_ref().is_some_and(|d| d.session == t.session))
        };
        if dialog {
            return None;
        }
        let probe = self.cdp().call_within(Some(&session), "Runtime.evaluate", json!({"expression": "1", "returnByValue": true}), Duration::from_secs(3)).await;
        if probe.is_ok() && !crashed {
            return None;
        }
        self.cdp().call_within(Some(&session), "Page.reload", json!({}), Duration::from_secs(10)).await.ok()?;
        let _ = self.wait_until(Duration::from_secs(15), |st| st.tab(target).is_none_or(|t| t.lifecycle.contains("load"))).await;
        Some("这个标签页没有响应（可能被浏览器回收或卡住了），已重新加载：原来的 ref 已失效，请重新 browser_snapshot".into())
    }

    pub fn tab_text(&self) -> String {
        self.tab_list()
            .into_iter()
            .map(|(id, url, title, cur)| format!("{}{id} {url}{}", if cur { "* " } else { "  " }, if title.is_empty() { String::new() } else { format!(" \"{}\"", crate::clip(&title, 100)) }))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// text / html / value / url / title / visible / enabled / checked of a ref or of the page
    pub async fn get(&self, kind: &str, r: Option<&str>) -> Result<String> {
        self.no_dialog()?;
        self.current()?;
        let v = match r {
            Some(r) => {
                if matches!(kind, "url" | "title") {
                    bail!("{kind} 只能读取页面，不需要 ref");
                }
                let t = self.resolve(r).await?;
                self.call_on(&t.session, &t.object, GET_JS, vec![json!(kind)]).await?
            }
            None => {
                let expr = match kind {
                    "text" => "document.body ? document.body.innerText : ''",
                    "html" => "document.documentElement.outerHTML",
                    "url" => "location.href",
                    "title" => "document.title",
                    _ => bail!("读取 {kind} 需要 ref"),
                };
                self.eval_doc(expr).await?
            }
        };
        Ok(match v {
            Value::String(s) => s,
            Value::Null => String::new(),
            v => v.to_string(),
        })
    }

    /// browser_get, with attr / box / styles of an element and count of a selector
    pub async fn get_ext(&self, kind: &str, r: Option<&str>, name: Option<&str>) -> Result<String> {
        match kind {
            "count" => {
                let sel = r.and_then(|r| r.strip_prefix("css=").map(|s| ("css", s)).or_else(|| r.strip_prefix("xpath=").map(|s| ("xpath", s)))).ok_or_else(|| anyhow!("count 需要 selector"))?;
                self.no_dialog()?;
                let expr = format!("(()=>{{const r=({SELECTOR_JS})({},{});return r?r.__n:0}})()", serde_json::to_string(sel.0)?, serde_json::to_string(sel.1)?);
                let n = self.eval_doc(&expr).await.map_err(|e| anyhow!("选择器无效：{e}"))?;
                Ok(format!("{} 个元素匹配 {}", n, sel.1))
            }
            "attr" | "box" | "styles" => {
                self.no_dialog()?;
                let r = r.ok_or_else(|| anyhow!("{kind} 需要 ref 或 selector"))?;
                if kind == "attr" && name.is_none() {
                    bail!("attr 需要 name（属性名，例如 href）");
                }
                let t = self.resolve(r).await?;
                let v = self.call_on(&t.session, &t.object, GET_JS, vec![json!(kind), json!(name)]).await?;
                Ok(match v {
                    Value::Null => format!("{} 没有属性 {}", t.r, name.unwrap_or("")),
                    Value::String(s) => s,
                    v => serde_json::to_string_pretty(&v)?,
                })
            }
            _ => self.get(kind, r).await,
        }
    }

    pub async fn wait(&self, a: &WaitArgs) -> Result<String> {
        let timeout = Duration::from_millis(a.timeout_ms.clamp(100, 60_000));
        if let Some(ms) = a.ms {
            let ms = ms.min(30_000);
            tokio::time::sleep(Duration::from_millis(ms)).await;
            return Ok(format!("已等待 {ms} 毫秒"));
        }
        if a.download {
            return self.wait_download(Duration::from_millis(a.timeout_ms.clamp(100, 300_000))).await;
        }
        if let Some(state) = &a.load {
            self.wait_load(state, timeout).await.map_err(|_| anyhow!("等待页面 {state} 超时"))?;
            self.no_dialog()?;
            return Ok(format!("页面已 {state}：{}", self.current_url()));
        }
        if let Some(pattern) = &a.url {
            let target = self.current()?.target_id;
            let p = pattern.clone();
            self.wait_until(timeout, |st| st.dialog.is_some() || st.tab(&target).is_some_and(|t| url_matches(&p, &t.url))).await.map_err(|_| anyhow!("等待地址匹配 {pattern} 超时，当前是 {}", self.current_url()))?;
            self.no_dialog()?;
            return Ok(format!("地址已是 {}", self.current_url()));
        }
        // Text, an element or a script condition: poll the page
        let state = a.state.as_deref().unwrap_or("visible");
        if !matches!(state, "visible" | "hidden" | "attached" | "detached") {
            bail!("state 只能是 visible、hidden、attached 或 detached");
        }
        let gone = matches!(state, "hidden" | "detached");
        let what = if let Some(t) = &a.text {
            format!("文本「{}」{}", crate::clip(t, 60), if gone { "消失" } else { "出现" })
        } else if let Some(r) = &a.r {
            format!("{} {}", target_label(r), match state { "hidden" => "隐藏或移除", "detached" => "从页面移除", "attached" => "出现在页面里", _ => "可见" })
        } else if let Some(f) = &a.function {
            format!("脚本条件 {} 成立", crate::clip(f, 80))
        } else {
            bail!("browser_wait 需要 ms、load、url、text、ref、selector、fn 或 download 之一");
        };
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            self.no_dialog()?;
            let done = if let Some(text) = &a.text {
                let expr = format!("(document.body ? document.body.innerText : '').includes({})", serde_json::to_string(text)?);
                let present = self.eval_doc(&expr).await.ok() == Some(json!(true));
                present != gone
            } else if let Some(r) = &a.r {
                match self.resolve(r).await {
                    Ok(t) => match state {
                        "attached" => true,
                        "detached" => self.call_on(&t.session, &t.object, "function(){return !this.isConnected}", vec![]).await.ok() == Some(json!(true)),
                        _ => {
                            let vis = self.call_on(&t.session, &t.object, GET_JS, vec![json!("visible")]).await.ok() == Some(json!(true));
                            vis != gone
                        }
                    },
                    // An element that's gone (or a ref of a page that navigated) counts as hidden / detached
                    Err(_) if gone => true,
                    Err(e) if e.to_string().contains("已失效") => return Err(e),
                    Err(_) => false,
                }
            } else if let Some(f) = &a.function {
                match self.eval_doc(&format!("(async()=>!!({f}))()")).await {
                    Ok(v) => v == json!(true),
                    Err(e) if e.to_string().contains("页面脚本出错") && tokio::time::Instant::now() + Duration::from_millis(250) >= deadline => return Err(e),
                    Err(_) => false,
                }
            } else {
                false
            };
            if done {
                return Ok(format!("条件已满足：{what}"));
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "等待{what}超时（{} 毫秒）。当前页面 {}；先 browser_snapshot 看看页面实际是什么样，不要盲目加长等待",
                    timeout.as_millis(),
                    self.current_url()
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub fn console(&self, level: Option<&str>, limit: usize, clear: bool) -> Result<String> {
        let mut st = self.state();
        let cur = st.current.clone().ok_or_else(|| anyhow!("没有打开的标签页"))?;
        let tab = st.tabs.iter_mut().find(|t| t.target_id == cur).ok_or_else(|| anyhow!("没有打开的标签页"))?;
        let lines: Vec<&String> = tab
            .console
            .iter()
            .filter(|l| match level {
                Some("error") => l.starts_with("[error]") || l.starts_with("[exception]") || l.starts_with("[assert]"),
                Some("warning") => l.starts_with("[warning]") || l.starts_with("[error]") || l.starts_with("[exception]"),
                _ => true,
            })
            .collect();
        let skip = lines.len().saturating_sub(limit.max(1));
        let text = if lines.is_empty() { "（没有控制台消息）".to_string() } else { lines[skip..].iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n") };
        if clear {
            tab.console.clear();
        }
        Ok(text)
    }

    pub async fn dialog(&self, action: &str, text: Option<&str>) -> Result<String> {
        let d = match self.pending_dialog() {
            Some(d) => d,
            None if action == "status" => return Ok("没有等待处理的对话框".into()),
            None => bail!("没有等待处理的对话框"),
        };
        if action == "status" {
            return Ok(dialog_line(&d));
        }
        let accept = match action {
            "accept" => true,
            "dismiss" => false,
            _ => bail!("action 只能是 accept、dismiss 或 status"),
        };
        let mut p = json!({"accept": accept});
        if let (true, Some(t)) = (accept && d.kind == "prompt", text) {
            p["promptText"] = json!(t);
        }
        let before = self.mark();
        self.cdp().call(Some(&d.session), "Page.handleJavaScriptDialog", p).await?;
        let _ = self.wait_until(Duration::from_secs(5), |st| st.dialog.is_none()).await;
        Ok(format!("已{} {} 对话框{}", if accept { "确认" } else { "取消" }, d.kind, self.after_action(&before).await))
    }

    /// PNG of the viewport or the whole page. With `labels`, numbered boxes are drawn over the refs of the latest
    /// snapshot first
    pub async fn screenshot(&self, full: bool, labels: &[(String, String)]) -> Result<Vec<u8>> {
        self.screenshot_as(full, labels, None, None).await
    }

    /// `element`: only that ref / selector. `jpeg`: JPEG at that quality instead of PNG (much smaller for photos)
    pub async fn screenshot_as(&self, full: bool, labels: &[(String, String)], element: Option<&str>, jpeg: Option<u8>) -> Result<Vec<u8>> {
        self.no_dialog()?;
        let tab = self.current()?;
        // Screenshots stay at CSS pixel size (the live view renders at 2x) unless the AI set a device scale
        let shrink = if self.state().emulation.scale.is_some() {
            1.0
        } else {
            let d = self.eval(&tab.session, "devicePixelRatio").await.ok().and_then(|v| v.as_f64()).unwrap_or(1.0);
            if d > 1.0 { 1.0 / d } else { 1.0 }
        };
        let clip = match element {
            Some(r) => {
                let t = self.resolve(r).await?;
                let _ = self.cdp().call(Some(&t.session), "DOM.scrollIntoViewIfNeeded", json!({"objectId": t.object})).await;
                let b = self.call_on(&t.session, &t.object, BOX_JS, vec![]).await?;
                if !b.is_object() {
                    bail!("{} 没有大小，无法截图", t.r);
                }
                // The clip is in page coordinates: add the scroll position
                let m = self.cdp().call(Some(&tab.session), "Page.getLayoutMetrics", json!({})).await?;
                let vv = &m["cssVisualViewport"];
                let f = |v: &Value| v.as_f64().unwrap_or(0.0);
                Some(json!({"x": f(&b["x"]) + f(&vv["pageX"]), "y": f(&b["y"]) + f(&vv["pageY"]), "width": f(&b["w"]).max(1.0), "height": f(&b["h"]).max(1.0), "scale": shrink}))
            }
            None => None,
        };
        if !labels.is_empty() {
            let mut boxes = vec![];
            for (n, r) in labels.iter().take(150) {
                let Ok(t) = self.resolve(r).await else { continue };
                if let Ok(v) = self.call_on(&t.session, &t.object, BOX_JS, vec![]).await {
                    if v.is_object() {
                        boxes.push(json!({"n": n, "x": v["x"], "y": v["y"], "w": v["w"], "h": v["h"]}));
                    }
                }
            }
            self.eval(&tab.session, &format!("({OVERLAY_JS})({})", Value::Array(boxes))).await?;
        }
        let mut p = json!({"format": "png"});
        if let Some(c) = clip {
            p["clip"] = c;
            p["captureBeyondViewport"] = json!(true);
        } else if full {
            let m = self.cdp().call(Some(&tab.session), "Page.getLayoutMetrics", json!({})).await?;
            let size = &m["cssContentSize"];
            let (w, h) = (size["width"].as_f64().unwrap_or(1280.0), size["height"].as_f64().unwrap_or(720.0).min(16_000.0));
            p = json!({"format": "png", "captureBeyondViewport": true, "clip": {"x": 0, "y": 0, "width": w, "height": h, "scale": shrink}});
        } else if shrink < 1.0 {
            let m = self.cdp().call(Some(&tab.session), "Page.getLayoutMetrics", json!({})).await?;
            let vv = &m["cssVisualViewport"];
            let f = |v: &Value| v.as_f64().unwrap_or(0.0);
            p["clip"] = json!({"x": f(&vv["pageX"]), "y": f(&vv["pageY"]), "width": f(&vv["clientWidth"]).max(1.0), "height": f(&vv["clientHeight"]).max(1.0), "scale": shrink});
        }
        if let Some(q) = jpeg {
            p["format"] = json!("jpeg");
            p["quality"] = json!(q.clamp(10, 100));
        }
        let r = self.cdp().call(Some(&tab.session), "Page.captureScreenshot", p).await;
        if !labels.is_empty() {
            let _ = self.eval(&tab.session, "document.getElementById('__yi_browser_labels')?.remove()").await;
        }
        let data = r?["data"].as_str().unwrap_or("").to_string();
        Ok(base64::engine::general_purpose::STANDARD.decode(data)?)
    }

    /// Whether this capture differs from the last one of the same scope, remembering it
    pub fn shot_changed(&self, scope: &str, png: &[u8]) -> bool {
        let digest = format!("{:x}", md5::compute(png));
        let mut st = self.state();
        let Some(cur) = st.current.clone() else { return true };
        let Some(tab) = st.tabs.iter_mut().find(|t| t.target_id == cur) else { return true };
        tab.shots.insert(scope.to_string(), digest.clone()) != Some(digest)
    }
}

/// Box of an element in tab coordinates (same-process frames included)
const BOX_JS: &str = r#"function(){
const r=this.getBoundingClientRect(); if(r.width===0&&r.height===0) return null;
let ox=0, oy=0; try{ let w=window; while(w.frameElement){ const f=w.frameElement.getBoundingClientRect(); ox+=f.left; oy+=f.top; w=w.parent; } }catch(e){}
return {x:r.left+ox, y:r.top+oy, w:r.width, h:r.height};
}"#;

const OVERLAY_JS: &str = r#"function(boxes){
const root=document.createElement('div'); root.id='__yi_browser_labels';
root.style.cssText='position:fixed;left:0;top:0;width:0;height:0;z-index:2147483647;pointer-events:none';
for(const b of boxes){
  const d=document.createElement('div');
  d.style.cssText=`position:fixed;left:${b.x}px;top:${b.y}px;width:${b.w}px;height:${b.h}px;outline:2px solid #e11d48;box-sizing:border-box`;
  const l=document.createElement('span'); l.textContent=b.n;
  l.style.cssText='position:absolute;left:-2px;top:-16px;background:#e11d48;color:#fff;font:bold 11px/14px sans-serif;padding:0 3px;border-radius:2px';
  d.appendChild(l); root.appendChild(d);
}
document.documentElement.appendChild(root);
return boxes.length;
}"#;

#[derive(Default)]
struct Mark {
    tab: String,
    url: String,
    navs: u64,
}

#[derive(Debug, Default, Clone)]
pub struct WaitArgs {
    pub ms: Option<u64>,
    pub load: Option<String>,
    pub url: Option<String>,
    pub text: Option<String>,
    pub r: Option<String>,
    /// visible (default) / hidden / attached / detached, for text and elements
    pub state: Option<String>,
    /// A JS expression polled until truthy
    pub function: Option<String>,
    pub download: bool,
    pub timeout_ms: u64,
}

/// CDP modifier bits: Alt 1, Control 2, Meta 4, Shift 8
pub fn modifier_mask(m: &[String]) -> Result<i64> {
    let mut v = 0;
    for k in m {
        v |= match k.to_ascii_lowercase().as_str() {
            "alt" | "option" => 1,
            "control" | "ctrl" => 2,
            "meta" | "cmd" | "command" => 4,
            "shift" => 8,
            other => bail!("未知的修饰键 {other}，可用 Alt、Control、Meta、Shift"),
        };
    }
    Ok(v)
}

/// How a navigation error reads, with what to do about the common ones on a dev machine
pub(crate) fn nav_hint(error: &str, url: &str) -> String {
    let local = ["//localhost", "//127.0.0.1", "//[::1]", "//0.0.0.0"].iter().any(|h| url.contains(h));
    if error.contains("ERR_CONNECTION_REFUSED") {
        if local {
            "。这个端口上没有服务在监听：开发服务器可能还没启动或换了端口，先启动它（或检查实际端口）再打开".into()
        } else {
            "。对方拒绝连接：地址或端口可能不对".into()
        }
    } else if error.contains("ERR_NAME_NOT_RESOLVED") {
        "。找不到这个域名：检查拼写，或这台主机没有网络".into()
    } else if error.contains("ERR_CERT") || error.contains("ERR_SSL") {
        "。证书无效：如果是自签名的开发证书，可以改用 http，或请用户在主机设置里打开「忽略证书错误」".into()
    } else if error.contains("ERR_BLOCKED_BY_CLIENT") {
        "。这个地址被主机设置的允许域名列表拦下了".into()
    } else if error.contains("ERR_UNSAFE_PORT") {
        "。Chrome 不允许访问这个端口（如 6000、6665–6669、10080），请让服务换一个端口".into()
    } else if error.contains("ERR_INVALID_AUTH_CREDENTIALS") {
        "。这个网站要求 HTTP 登录（用户名和密码）：知道的话用 browser_set credentials 设置后再打开，否则请用户提供".into()
    } else if error.contains("ERR_INTERNET_DISCONNECTED") {
        "。浏览器处于离线状态（browser_set offline=false 可恢复）".into()
    } else if error.contains("ERR_TIMED_OUT") || error.contains("ERR_CONNECTION_TIMED_OUT") {
        "。连接超时：服务可能卡住了或在防火墙后面".into()
    } else {
        String::new()
    }
}

/// How a ref or selector argument is named in messages
pub(crate) fn target_label(r: &str) -> String {
    match r.strip_prefix("css=").or_else(|| r.strip_prefix("xpath=")) {
        Some(s) => format!("选择器 {}", crate::clip(s, 80)),
        None => r.trim_start_matches('@').to_string(),
    }
}

pub(crate) fn dialog_line(d: &crate::browser::Dialog) -> String {
    let mut s = format!("页面正在等待 {} 对话框：{}", d.kind, crate::clip(&d.message, 500));
    if d.kind == "prompt" && !d.default_prompt.is_empty() {
        s.push_str(&format!("（默认值：{}）", d.default_prompt));
    }
    s.push_str("。请用 browser_dialog 选择 accept 或 dismiss");
    s
}

/// Only http(s) as a top-level address; a bare host gets https://, a bare port means localhost
pub fn check_url(input: &str) -> Result<String> {
    let v = input.trim();
    if v.is_empty() {
        bail!("缺少网址");
    }
    let lower = v.to_ascii_lowercase();
    let full = if lower.starts_with("http://") || lower.starts_with("https://") {
        v.to_string()
    } else if let Some((scheme, _)) = v.split_once(':').filter(|(s, rest)| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)) && !rest.chars().next().is_some_and(|c| c.is_ascii_digit())) {
        bail!("只能打开 http 和 https 地址，不能打开 {scheme}: 地址");
    } else if v.chars().all(|c| c.is_ascii_digit()) {
        format!("http://localhost:{v}")
    } else if ["localhost", "127.0.0.1", "[::1]", "0.0.0.0"].iter().any(|h| lower == *h || lower.starts_with(&format!("{h}:")) || lower.starts_with(&format!("{h}/"))) {
        format!("http://{v}")
    } else {
        format!("https://{v}")
    };
    if full.contains(char::is_whitespace) {
        bail!("网址里不能有空白字符");
    }
    Ok(full)
}

/// `*` matches any run of characters; without `*` the pattern is a substring
pub fn url_matches(pattern: &str, url: &str) -> bool {
    if !pattern.contains('*') {
        return url.contains(pattern);
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = url;
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() {
            continue;
        }
        if i == 0 {
            match rest.strip_prefix(p) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if let Some(at) = rest.find(p) {
            rest = &rest[at + p.len()..];
        } else {
            return false;
        }
    }
    parts.last().is_some_and(|l| l.is_empty() || url.ends_with(l))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(check_url("example.com").unwrap(), "https://example.com");
        assert_eq!(check_url("3000").unwrap(), "http://localhost:3000");
        assert_eq!(check_url("localhost:5173/x").unwrap(), "http://localhost:5173/x");
        assert_eq!(check_url("HTTP://a.b").unwrap(), "HTTP://a.b");
        for bad in ["file:///etc/passwd", "javascript:alert(1)", "chrome://settings", "data:text/html,x", "tauri://localhost", "about:blank", ""] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
        assert!(url_matches("/done", "http://x/done?a"));
        assert!(url_matches("http://x/*/ok", "http://x/a/b/ok"));
        assert!(!url_matches("http://x/*/ok", "http://x/a/b/no"));
        assert!(url_matches("*example*", "https://www.example.com/"));
    }
}
