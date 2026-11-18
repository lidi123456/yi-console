//! Accessibility snapshots with `[ref=eN]`, modeled on agent-browser's `snapshot.rs` (Apache-2.0, Copyright
//! Vercel, Inc.): role lists, one level of inline iframes, hints for unnamed or duplicate controls.

use crate::browser::{Browser, RefEntry};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;

pub const INTERACTIVE_ROLES: &[&str] = &[
    "button", "link", "textbox", "checkbox", "radio", "combobox", "listbox", "menuitem", "menuitemcheckbox", "menuitemradio",
    "option", "searchbox", "slider", "spinbutton", "switch", "tab", "treeitem", "Iframe",
];
const CELL_ROLES: &[&str] = &["cell", "gridcell", "columnheader", "rowheader"];
const CONTENT_ROLES: &[&str] = &["heading", "listitem", "article", "region", "main", "navigation"];
/// Containers that only matter through their children
const STRUCTURAL_ROLES: &[&str] = &[
    "generic", "group", "list", "table", "row", "rowgroup", "grid", "treegrid", "menu", "menubar", "toolbar", "tablist", "tree",
    "directory", "document", "application", "presentation", "none", "WebArea", "RootWebArea", "Section", "paragraph", "LabelText",
    "ListMarker", "DescriptionList", "Div", "Pre", "Canvas", "figure", "blockquote", "Abbr", "Ruby", "time", "emphasis", "strong",
    "subscript", "superscript", "code", "deletion", "insertion", "mark", "contentinfo", "banner", "complementary", "form", "search",
];
const MAX_HINTS: usize = 60;

#[derive(Debug, Clone, Default)]
pub struct SnapshotOptions {
    pub interactive: bool,
    pub compact: bool,
    pub depth: Option<usize>,
    pub selector: Option<String>,
    /// Only the subtree of this ref
    pub scope: Option<String>,
    pub urls: bool,
}

impl SnapshotOptions {
    /// Options that change what a snapshot shows, so browser_diff only compares like with like
    pub fn key(&self) -> String {
        format!("{}|{}|{:?}|{:?}|{:?}|{}", self.interactive, self.compact, self.depth, self.selector, self.scope, self.urls)
    }
}

/// A DOM element the accessibility tree doesn't call interactive, found by `CURSOR_JS`: `<div onClick>`, cards with
/// cursor:pointer, contenteditable, tabindex; or a visible label standing for a hidden radio / checkbox
#[derive(Debug, Clone)]
pub(crate) struct Extra {
    pub kind: String,
    pub label: String,
    pub checked: Option<bool>,
}

/// Elements that act as controls without a role (agent-browser's `find_cursor_interactive_elements` and
/// `promote_hidden_inputs`). Returns `{els, info}` by reference; nothing is written to the DOM
const CURSOR_JS: &str = r#"(()=>{
const out=[],info=[],max=150;
const native=new Set(['A','BUTTON','INPUT','SELECT','TEXTAREA','SUMMARY','OPTION','LABEL','IFRAME','VIDEO','AUDIO','DETAILS','OPTGROUP']);
const iroles=new Set(['button','link','textbox','checkbox','radio','combobox','listbox','menuitem','menuitemcheckbox','menuitemradio','option','searchbox','slider','spinbutton','switch','tab','treeitem']);
const vis=el=>{const r=el.getBoundingClientRect(); if(r.width<2||r.height<2) return false; const s=getComputedStyle(el); return s.visibility!=='hidden'&&s.display!=='none'&&parseFloat(s.opacity)>0;};
const txt=el=>(el.getAttribute('aria-label')||el.getAttribute('title')||el.innerText||el.textContent||'').replace(/[\s ]+/g,' ').trim().slice(0,80);
const inside=el=>{for(let p=el.parentElement;p;p=p.parentElement){ if(native.has(p.tagName)||iroles.has(p.getAttribute('role'))) return true;} return false;};
const taken=new Set();
for(const inp of document.querySelectorAll('input[type=radio],input[type=checkbox]')){
  if(out.length>=max) break;
  if(vis(inp)) continue;
  const l=inp.labels&&[...inp.labels].find(vis);
  const box=l||(inp.parentElement&&vis(inp.parentElement)?inp.parentElement:null);
  if(!box||taken.has(box)) continue;
  taken.add(box); out.push(box); info.push({kind:inp.type, label:txt(box), checked:inp.checked});
}
for(const el of (document.body?document.body.querySelectorAll('*'):[])){
  if(out.length>=max) break;
  if(native.has(el.tagName)||taken.has(el)) continue;
  const role=el.getAttribute('role'); if(role&&iroles.has(role)) continue;
  let kind='';
  if(el.isContentEditable&&!(el.parentElement&&el.parentElement.isContentEditable)) kind='editable';
  else if(el.hasAttribute('onclick')||typeof el.onclick==='function') kind='clickable';
  else if(getComputedStyle(el).cursor==='pointer'&&!(el.parentElement&&getComputedStyle(el.parentElement).cursor==='pointer')) kind='clickable';
  else if(el.hasAttribute('tabindex')&&el.tabIndex>=0) kind='focusable';
  if(!kind||inside(el)||!vis(el)) continue;
  taken.add(el); out.push(el); info.push({kind, label:txt(el)});
}
return {els:out, info};
})()"#;

#[derive(Debug, Clone)]
pub(crate) struct AxNode {
    pub role: String,
    pub name: String,
    pub value: String,
    pub ignored: bool,
    pub props: HashMap<String, Value>,
    pub children: Vec<String>,
    pub backend: Option<i64>,
}

pub(crate) fn parse_ax(nodes: &Value) -> (HashMap<String, AxNode>, Option<String>) {
    let mut map = HashMap::new();
    let mut root = None;
    for n in nodes.as_array().into_iter().flatten() {
        let id = n["nodeId"].as_str().unwrap_or("").to_string();
        if root.is_none() && n["parentId"].is_null() {
            root = Some(id.clone());
        }
        let props = n["properties"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some((p["name"].as_str()?.to_string(), p["value"]["value"].clone())))
            .collect();
        map.insert(
            id,
            AxNode {
                role: n["role"]["value"].as_str().unwrap_or("").to_string(),
                name: clean(n["name"]["value"].as_str().unwrap_or("")),
                value: n["value"]["value"].as_str().map(clean).unwrap_or_else(|| match &n["value"]["value"] {
                    Value::Null => String::new(),
                    v => v.to_string(),
                }),
                ignored: n["ignored"].as_bool().unwrap_or(false),
                props,
                children: n["childIds"].as_array().into_iter().flatten().filter_map(|c| c.as_str().map(String::from)).collect(),
                backend: n["backendDOMNodeId"].as_i64(),
            },
        );
    }
    (map, root)
}

/// Collapse whitespace and drop zero-width characters
fn clean(s: &str) -> String {
    let s: String = s.chars().filter(|c| !matches!(c, '\u{FEFF}' | '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}')).collect();
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn wants_ref(role: &str, name: &str) -> bool {
    INTERACTIVE_ROLES.contains(&role) || ((CELL_ROLES.contains(&role) || CONTENT_ROLES.contains(&role)) && !name.is_empty())
}

/// One frame's tree, with where it came from
pub(crate) struct FrameTree {
    pub session: String,
    pub frame: String,
    pub nodes: HashMap<String, AxNode>,
    pub root: Option<String>,
}

/// A rendered line before refs are known: tree position plus what the ref needs
pub(crate) struct Line {
    pub depth: usize,
    pub role: String,
    pub name: String,
    pub attrs: String,
    pub target: Option<(String, String, i64)>,
    pub text: bool,
}

struct Ctx<'a> {
    opts: &'a SnapshotOptions,
    out: Vec<Line>,
    extra: &'a HashMap<i64, Extra>,
}

/// Lines of a subtree. Returns whether anything was printed (for compact mode)
fn walk(t: &FrameTree, id: &str, depth: usize, parent_name: &str, ctx: &mut Ctx, iframes: &mut Vec<(usize, i64)>) -> bool {
    let Some(n) = t.nodes.get(id) else { return false };
    if ctx.opts.depth.is_some_and(|d| depth > d) {
        return false;
    }
    let extra = n.backend.and_then(|b| ctx.extra.get(&b)).filter(|_| !INTERACTIVE_ROLES.contains(&n.role.as_str()));
    if let Some(x) = extra {
        return walk_extra(t, n, x, depth, ctx, iframes);
    }
    let role = n.role.as_str();
    let skip = n.ignored || matches!(role, "InlineTextBox" | "LineBreak") || (role == "StaticText" && (ctx.opts.interactive || n.name.is_empty() || n.name == parent_name));
    if skip {
        // Ignored nodes still have visible children
        if n.ignored {
            let mut any = false;
            for c in &n.children {
                any |= walk(t, c, depth, parent_name, ctx, iframes);
            }
            return any;
        }
        return false;
    }
    let refd = wants_ref(role, &n.name);
    let print = if ctx.opts.interactive {
        INTERACTIVE_ROLES.contains(&role)
    } else if role == "StaticText" {
        true
    } else if STRUCTURAL_ROLES.contains(&role) {
        !n.name.is_empty() && !matches!(role, "generic" | "none" | "presentation" | "RootWebArea" | "WebArea" | "paragraph" | "Section" | "LabelText")
    } else {
        true
    };
    let start = ctx.out.len();
    if print {
        ctx.out.push(Line {
            depth,
            role: if role == "StaticText" { "text".into() } else { role.into() },
            name: n.name.clone(),
            attrs: attrs(n, ctx.opts),
            target: if refd { n.backend.map(|b| (t.session.clone(), t.frame.clone(), b)) } else { None },
            text: role == "StaticText",
        });
        if role == "Iframe" {
            if let Some(b) = n.backend {
                iframes.push((start, b));
            }
        }
    }
    let child_depth = if print { depth + 1 } else { depth };
    let name = if n.name.is_empty() { parent_name } else { n.name.as_str() };
    let mut any = false;
    for c in &n.children {
        any |= walk(t, c, child_depth, name, ctx, iframes);
    }
    // Compact: drop printed containers with nothing inside and nothing of their own
    if print && ctx.opts.compact && !any && !refd && n.name.is_empty() && n.value.is_empty() && role != "StaticText" {
        ctx.out.truncate(start);
        return false;
    }
    print || any
}

/// A control found by `CURSOR_JS`: printed (and given a ref) whatever its accessibility role, children below it
fn walk_extra(t: &FrameTree, n: &AxNode, x: &Extra, depth: usize, ctx: &mut Ctx, iframes: &mut Vec<(usize, i64)>) -> bool {
    let name = if n.name.is_empty() { x.label.clone() } else { n.name.clone() };
    let mut attrs = match x.checked {
        Some(true) => "[checked]".to_string(),
        _ => String::new(),
    };
    if x.kind == "clickable" || x.kind == "focusable" || x.kind == "editable" {
        attrs = format!("{attrs}{}[{}]", if attrs.is_empty() { "" } else { " " }, if x.kind == "editable" { "contenteditable" } else { "no role" });
    }
    ctx.out.push(Line { depth, role: x.kind.clone(), name: name.clone(), attrs, target: n.backend.map(|b| (t.session.clone(), t.frame.clone(), b)), text: false });
    // Children under it: controls inside a card, and text that isn't just its name
    for c in &n.children {
        walk(t, c, depth + 1, &name, ctx, iframes);
    }
    true
}

/// Adjacent text lines at one depth (a sentence split by <b>, <span> ...) become one
pub(crate) fn merge_text(lines: Vec<Line>) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::with_capacity(lines.len());
    for l in lines {
        if let Some(prev) = out.last_mut() {
            if prev.text && l.text && prev.depth == l.depth {
                // Names are trimmed, so the space between the pieces is guessed: none around CJK or before punctuation
                let wide = |c: Option<char>| c.is_some_and(|c| !c.is_ascii());
                let glue = if wide(prev.name.chars().last()) || wide(l.name.chars().next()) || l.name.starts_with(|c: char| ",.;:!?)".contains(c)) { "" } else { " " };
                prev.name = format!("{}{glue}{}", prev.name, l.name);
                continue;
            }
        }
        out.push(l);
    }
    out
}

fn attrs(n: &AxNode, o: &SnapshotOptions) -> String {
    let mut a = vec![];
    let flag = |k: &str| n.props.get(k).is_some_and(|v| v == &json!(true) || v == "true");
    match n.props.get("checked").and_then(|v| v.as_str().map(String::from).or_else(|| v.as_bool().map(|b| b.to_string()))) {
        Some(c) if c == "true" => a.push("[checked]".to_string()),
        Some(c) if c == "mixed" => a.push("[checked=mixed]".into()),
        _ => {}
    }
    for k in ["disabled", "selected", "required", "readonly"] {
        if flag(k) {
            a.push(format!("[{k}]"));
        }
    }
    if let Some(e) = n.props.get("expanded").and_then(Value::as_bool) {
        a.push(format!("[expanded={e}]"));
    }
    if let Some(l) = n.props.get("level").and_then(Value::as_i64) {
        a.push(format!("[level={l}]"));
    }
    if !n.value.is_empty() && n.value != n.name {
        a.push(format!("value={}", quote(&crate::clip(&n.value, 200))));
    }
    if o.urls && n.role == "link" {
        if let Some(u) = n.props.get("url").and_then(Value::as_str) {
            a.push(format!("url={u}"));
        }
    }
    a.join(" ")
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// Final text: `- role "name" [ref=eN] attrs hint="..."`
pub(crate) fn format_lines(lines: &[Line], refs: &[Option<String>], hints: &HashMap<usize, String>) -> String {
    let mut s = String::new();
    for (i, l) in lines.iter().enumerate() {
        s.push_str(&"  ".repeat(l.depth));
        s.push_str("- ");
        if l.text {
            s.push_str("text: ");
            s.push_str(&crate::clip(&l.name, 300));
        } else {
            s.push_str(&l.role);
            if !l.name.is_empty() {
                s.push(' ');
                s.push_str(&quote(&crate::clip(&l.name, 150)));
            }
            if let Some(Some(r)) = refs.get(i) {
                s.push_str(&format!(" [ref={r}]"));
            }
            if !l.attrs.is_empty() {
                s.push(' ');
                s.push_str(&l.attrs);
            }
            if let Some(h) = hints.get(&i) {
                s.push_str(&format!(" hint={}", quote(h)));
            }
        }
        s.push('\n');
    }
    s
}

const HINT_JS: &str = r#"function(){
const el=this, parts=[];
const icon=(()=>{
  const u=el.querySelector&&el.querySelector('use'); const h=u&&(u.getAttribute('href')||u.getAttribute('xlink:href'));
  if(h&&h.startsWith('#')) return h.slice(1);
  const s=el.matches&&el.matches('svg')?el:(el.querySelector&&el.querySelector('svg'));
  if(s){ if(s.id) return s.id; if(s.dataset&&s.dataset.icon) return s.dataset.icon; }
  const nodes=[el,...(el.querySelectorAll?el.querySelectorAll('*'):[])].slice(0,20);
  for(const n of nodes){ for(const c of (n.classList||[])){ if(/^(lucide-|fa-|bi-|icon-|mdi-|ti-|ri-|codicon-)/.test(c)&&!/^fa-(solid|regular|brands|light|thin|duotone|sharp)$/.test(c)) return c; } }
  return '';
})();
if(icon) parts.push('icon:'+icon);
const img=el.matches&&el.matches('img')?el:(el.querySelector&&el.querySelector('img'));
if(img&&img.src){ const f=(img.src.split(/[?#]/)[0].split('/').pop()||'').replace(/\.[a-z0-9]+$/i,''); if(f&&!f.startsWith('data:')) parts.push('img:'+f.slice(0,40)); }
for(const a of ['data-testid','name','id']){ const v=el.getAttribute&&el.getAttribute(a); if(v&&!/\d{4,}|^[a-f0-9-]{16,}$|^:r|^radix-|^headlessui-/.test(v)){ parts.push(a+':'+v.slice(0,40)); break; } }
const box=el.closest&&el.closest('tr,li,article');
if(box&&box!==el){ const t=(box.innerText||'').replace(/\s+/g,' ').trim().slice(0,40); if(t) parts.push('in:'+t); }
return parts.join('; ');
}"#;

impl Browser {
    /// The accessibility tree of a frame: `frame` None is the session's own main frame
    pub(crate) async fn frame_tree(&self, session: &str, frame: &str, own: bool) -> Result<FrameTree> {
        let params = if own { json!({}) } else { json!({"frameId": frame}) };
        let r = self.cdp().call(Some(session), "Accessibility.getFullAXTree", params).await?;
        let (nodes, root) = parse_ax(&r["nodes"]);
        Ok(FrameTree { session: session.into(), frame: frame.into(), nodes, root })
    }

    /// Where an `<iframe>` element's document lives: (session, frame id, whether it is that session's own root)
    pub(crate) async fn child_frame(&self, session: &str, backend: i64) -> Option<(String, String, bool)> {
        let d = self.cdp().call(Some(session), "DOM.describeNode", json!({"backendNodeId": backend})).await.ok()?;
        let fid = d["node"]["frameId"].as_str()?.to_string();
        if let Some(s) = self.state().oopif.get(&fid).cloned() {
            return Some((s, fid, true));
        }
        Some((session.to_string(), fid, false))
    }

    pub async fn snapshot(&self, o: &SnapshotOptions) -> Result<String> {
        let tab = self.current()?;
        // browser_frame picked an iframe: its document is the root
        let (doc_session, doc_frame, own) = match &tab.frame {
            Some(f) => (f.session.clone(), f.frame.clone(), f.own),
            None => (tab.session.clone(), tab.main_frame.clone(), true),
        };
        let doc = self.doc()?;
        let mut main = self.frame_tree(&doc_session, &doc_frame, own).await?;
        if main.frame.is_empty() {
            main.frame = doc_frame.clone();
        }
        let mut root = main.root.clone();
        if let Some(sel) = o.selector.as_deref().filter(|s| !s.is_empty()) {
            let mut q = json!({"expression": format!("document.querySelector({})", quote(sel))});
            if let Some(c) = doc.context {
                q["contextId"] = json!(c);
            }
            let r = self.cdp().call(Some(&doc_session), "Runtime.evaluate", q).await?;
            let obj = r["result"]["objectId"].as_str().ok_or_else(|| anyhow!("没有找到匹配 {sel} 的元素"))?;
            let d = self.cdp().call(Some(&doc_session), "DOM.describeNode", json!({"objectId": obj})).await?;
            let b = d["node"]["backendNodeId"].as_i64();
            root = main.nodes.iter().find(|(_, n)| n.backend == b).map(|(k, _)| k.clone());
            if root.is_none() {
                anyhow::bail!("{sel} 没有可访问的内容");
            }
        }
        if let Some(r) = o.scope.as_deref().map(|r| r.trim().trim_start_matches('@')).filter(|r| !r.is_empty()) {
            let e = self.state().refs.map.get(r).cloned().ok_or_else(|| anyhow!("没有 ref {r}，请先 browser_snapshot"))?;
            if !e.valid || e.tab != tab.target_id {
                anyhow::bail!("{r} 已失效，请重新 browser_snapshot");
            }
            if e.session != doc_session {
                anyhow::bail!("{r} 在另一个框架里：先 browser_frame 进入那个框架再按 ref 截取");
            }
            root = main.nodes.iter().find(|(_, n)| n.backend == Some(e.backend)).map(|(k, _)| k.clone());
            if root.is_none() {
                anyhow::bail!("{r} 没有可访问的内容");
            }
        }
        let extra = self.cursor_elements(&doc_session, doc.context).await.unwrap_or_default();
        let mut ctx = Ctx { opts: o, out: vec![], extra: &extra };
        let mut iframes = vec![];
        if let Some(r) = &root {
            walk(&main, r, 0, "", &mut ctx, &mut iframes);
        }
        // Inline one level of iframes right under their node
        let mut lines = ctx.out;
        for (at, backend) in iframes.into_iter().rev() {
            let Some((sess, fid, own)) = self.child_frame(&doc_session, backend).await else { continue };
            let Ok(t) = self.frame_tree(&sess, &fid, own).await else { continue };
            let Some(r) = t.root.clone() else { continue };
            let depth = lines[at].depth + 1;
            let none = HashMap::new();
            let mut c = Ctx { opts: o, out: vec![], extra: &none };
            let mut nested = vec![];
            walk(&t, &r, depth, "", &mut c, &mut nested);
            lines.splice(at + 1..at + 1, c.out);
        }
        if !o.interactive {
            lines = merge_text(lines);
        }
        // Refs: the same node keeps its ref; role+name uniqueness decides fallback matching and hints
        let mut counts: HashMap<(String, String), usize> = HashMap::new();
        for l in lines.iter().filter(|l| l.target.is_some()) {
            *counts.entry((l.role.clone(), l.name.clone())).or_default() += 1;
        }
        let mut refs = vec![];
        let mut order = vec![];
        {
            let mut st = self.state();
            for l in &lines {
                let r = l.target.as_ref().map(|(s, f, b)| {
                    let unique = counts.get(&(l.role.clone(), l.name.clone())) == Some(&1);
                    st.refs.assign(RefEntry {
                        tab: tab.target_id.clone(),
                        session: s.clone(),
                        frame: f.clone(),
                        backend: *b,
                        role: l.role.clone(),
                        name: l.name.clone(),
                        unique,
                        valid: true,
                    })
                });
                if let Some(r) = &r {
                    order.push(r.clone());
                }
                refs.push(r);
            }
            st.refs.latest.insert(tab.target_id.clone(), order);
        }
        let mut hints = HashMap::new();
        for (i, l) in lines.iter().enumerate() {
            if hints.len() >= MAX_HINTS {
                break;
            }
            let Some((s, _, b)) = &l.target else { continue };
            if l.role == "Iframe" || !(l.name.is_empty() || counts.get(&(l.role.clone(), l.name.clone())).is_some_and(|c| *c > 1)) {
                continue;
            }
            if let Ok(obj) = self.object_for(s, *b).await {
                if let Ok(Value::String(h)) = self.call_on(s, &obj, HINT_JS, vec![]).await {
                    if !h.is_empty() {
                        hints.insert(i, h);
                    }
                }
            }
        }
        let text = format_lines(&lines, &refs, &hints);
        Ok(if text.is_empty() { "（页面没有可显示的内容）\n".into() } else { text })
    }

    /// Run an expression returning `{els, info}` and get each element's object id with its info entry. `group`:
    /// object group to put them in (released by the caller), else they live until the page drops them
    pub(crate) async fn collect_elements_in(&self, session: &str, context: Option<i64>, expr: &str, group: Option<&str>) -> Result<Vec<(String, Value)>> {
        let mut q = json!({"expression": expr});
        if let Some(c) = context {
            q["contextId"] = json!(c);
        }
        if let Some(g) = group {
            q["objectGroup"] = json!(g);
        }
        let v = self.cdp().call(Some(session), "Runtime.evaluate", q).await?;
        if let Some(e) = v.get("exceptionDetails") {
            anyhow::bail!("页面脚本出错：{}", e["exception"]["description"].as_str().or(e["text"].as_str()).unwrap_or(""));
        }
        let obj = v["result"]["objectId"].as_str().ok_or_else(|| anyhow!("no result"))?.to_string();
        let info = self
            .cdp()
            .call(Some(session), "Runtime.callFunctionOn", json!({"objectId": obj, "functionDeclaration": "function(){return this.info}", "returnByValue": true}))
            .await?["result"]["value"]
            .clone();
        let mut p = json!({"objectId": obj, "functionDeclaration": "function(){return this.els}"});
        if let Some(g) = group {
            p["objectGroup"] = json!(g);
        }
        let els = self.cdp().call(Some(session), "Runtime.callFunctionOn", p).await?;
        let els = els["result"]["objectId"].as_str().ok_or_else(|| anyhow!("no elements"))?.to_string();
        let props = self.cdp().call(Some(session), "Runtime.getProperties", json!({"objectId": els, "ownProperties": true})).await?;
        let mut out = vec![];
        for p in props["result"].as_array().into_iter().flatten() {
            let Some(i) = p["name"].as_str().and_then(|n| n.parse::<usize>().ok()) else { continue };
            let Some(o) = p["value"]["objectId"].as_str() else { continue };
            out.push((i, o.to_string(), info[i].clone()));
        }
        out.sort_by_key(|(i, _, _)| *i);
        Ok(out.into_iter().map(|(_, o, i)| (o, i)).collect())
    }

    pub(crate) async fn collect_elements(&self, session: &str, context: Option<i64>, expr: &str) -> Result<Vec<(String, Value)>> {
        self.collect_elements_in(session, context, expr, None).await
    }

    /// `CURSOR_JS` in a document, by backend node id
    pub(crate) async fn cursor_elements(&self, session: &str, context: Option<i64>) -> Result<HashMap<i64, Extra>> {
        let group = "yi-cursor";
        let r = async {
            let mut out = HashMap::new();
            for (o, x) in self.collect_elements_in(session, context, CURSOR_JS, Some(group)).await? {
                let d = self.cdp().call(Some(session), "DOM.describeNode", json!({"objectId": o})).await?;
                let Some(b) = d["node"]["backendNodeId"].as_i64() else { continue };
                out.insert(b, Extra { kind: x["kind"].as_str().unwrap_or("clickable").into(), label: x["label"].as_str().unwrap_or("").into(), checked: x["checked"].as_bool() });
            }
            Ok::<_, anyhow::Error>(out)
        }
        .await;
        let _ = self.cdp().call(Some(session), "Runtime.releaseObjectGroup", json!({"objectGroup": group})).await;
        r
    }

    /// browser_find: elements by role, text, label, placeholder, alt, title, test id or CSS, given refs
    pub async fn find(&self, by: &str, value: &str, name: Option<&str>, exact: bool) -> Result<String> {
        let tab = self.current()?;
        let (doc_session, doc_frame, own) = match &tab.frame {
            Some(f) => (f.session.clone(), f.frame.clone(), f.own),
            None => (tab.session.clone(), tab.main_frame.clone(), true),
        };
        let doc = self.doc()?;
        let tree = self.frame_tree(&doc_session, &doc_frame, own).await?;
        let by_backend: HashMap<i64, &AxNode> = tree.nodes.values().filter(|n| !n.ignored).filter_map(|n| n.backend.map(|b| (b, n))).collect();
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        let matches = |have: &str, want: &str| if exact { norm(have) == norm(want) } else { norm(have).contains(&norm(want)) };
        // (backend, role, name, extra text)
        let mut found: Vec<(i64, String, String, String)> = vec![];
        if by == "role" {
            for n in tree.nodes.values() {
                let Some(b) = n.backend else { continue };
                if n.ignored || !n.role.eq_ignore_ascii_case(value) || name.is_some_and(|w| !matches(&n.name, w)) {
                    continue;
                }
                found.push((b, n.role.clone(), n.name.clone(), String::new()));
            }
            found.sort_by_key(|f| f.0);
            found.truncate(30);
        } else {
            if !matches!(by, "text" | "label" | "placeholder" | "alt" | "title" | "testid" | "css") {
                anyhow::bail!("by 只能是 role、text、label、placeholder、alt、title、testid 或 css");
            }
            let expr = format!("({})({},{},{})", crate::actions::FIND_JS, quote(by), quote(value), exact);
            for (o, i) in self.collect_elements(&doc.session, doc.context, &expr).await.map_err(|e| anyhow!("查找失败：{e}"))? {
                let d = self.cdp().call(Some(&doc.session), "DOM.describeNode", json!({"objectId": o})).await?;
                let Some(b) = d["node"]["backendNodeId"].as_i64() else { continue };
                let (role, nm) = by_backend.get(&b).map(|n| (n.role.clone(), n.name.clone())).unwrap_or_else(|| (i["tag"].as_str().unwrap_or("").to_string(), String::new()));
                let extra = format!("<{}>{}", i["tag"].as_str().unwrap_or(""), if i["visible"] == json!(false) { " [hidden]" } else { "" });
                let nm = if nm.is_empty() { i["text"].as_str().unwrap_or("").to_string() } else { nm };
                found.push((b, role, nm, extra));
            }
        }
        if found.is_empty() {
            return Ok(format!("没有找到 {by}={value} 的元素"));
        }
        let mut st = self.state();
        let lines: Vec<String> = found
            .into_iter()
            .map(|(b, role, nm, extra)| {
                let r = st.refs.assign(RefEntry { tab: tab.target_id.clone(), session: doc_session.clone(), frame: doc_frame.clone(), backend: b, role: role.clone(), name: nm.clone(), unique: false, valid: true });
                format!("- {role}{} [ref={r}]{}", if nm.is_empty() { String::new() } else { format!(" {}", quote(&crate::clip(&nm, 120))) }, if extra.is_empty() { String::new() } else { format!(" {extra}") })
            })
            .collect();
        Ok(lines.join("\n"))
    }

    pub(crate) async fn object_for(&self, session: &str, backend: i64) -> Result<String> {
        let r = self.cdp().call(Some(session), "DOM.resolveNode", json!({"backendNodeId": backend})).await?;
        r["object"]["objectId"].as_str().map(String::from).ok_or_else(|| anyhow!("元素已不存在"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(nodes: Value) -> FrameTree {
        let (nodes, root) = parse_ax(&nodes);
        FrameTree { session: "s".into(), frame: "f".into(), nodes, root }
    }

    fn sample() -> FrameTree {
        tree(json!([
            {"nodeId": "1", "role": {"value": "RootWebArea"}, "name": {"value": "Login"}, "childIds": ["2", "3"]},
            {"nodeId": "2", "parentId": "1", "role": {"value": "navigation"}, "name": {"value": ""}, "childIds": ["4"]},
            {"nodeId": "4", "parentId": "2", "role": {"value": "link"}, "name": {"value": "Home"}, "backendDOMNodeId": 10, "childIds": ["5"]},
            {"nodeId": "5", "parentId": "4", "role": {"value": "StaticText"}, "name": {"value": "Home"}},
            {"nodeId": "3", "parentId": "1", "role": {"value": "generic"}, "ignored": false, "childIds": ["6", "7", "8"]},
            {"nodeId": "6", "parentId": "3", "role": {"value": "textbox"}, "name": {"value": "User"}, "value": {"value": "ann"}, "backendDOMNodeId": 11},
            {"nodeId": "7", "parentId": "3", "role": {"value": "checkbox"}, "name": {"value": "Remember"}, "backendDOMNodeId": 12,
             "properties": [{"name": "checked", "value": {"type": "tristate", "value": "true"}}]},
            {"nodeId": "8", "parentId": "3", "role": {"value": "StaticText"}, "name": {"value": "Forgot   password?"}}
        ]))
    }

    fn render(t: &FrameTree, o: SnapshotOptions) -> Vec<Line> {
        render_with(t, o, &HashMap::new())
    }

    fn render_with(t: &FrameTree, o: SnapshotOptions, extra: &HashMap<i64, Extra>) -> Vec<Line> {
        let mut ctx = Ctx { opts: &o, out: vec![], extra };
        let mut iframes = vec![];
        walk(t, t.root.as_deref().unwrap(), 0, "", &mut ctx, &mut iframes);
        ctx.out
    }

    #[test]
    fn found_controls_get_refs_and_text_merges() {
        let t = tree(json!([
            {"nodeId": "1", "role": {"value": "RootWebArea"}, "childIds": ["2", "4", "6"]},
            {"nodeId": "2", "parentId": "1", "role": {"value": "generic"}, "ignored": true, "backendDOMNodeId": 20, "childIds": ["3"]},
            {"nodeId": "3", "parentId": "2", "role": {"value": "StaticText"}, "name": {"value": "Open card"}},
            {"nodeId": "4", "parentId": "1", "role": {"value": "LabelText"}, "backendDOMNodeId": 21, "childIds": ["5"]},
            {"nodeId": "5", "parentId": "4", "role": {"value": "StaticText"}, "name": {"value": "Pro plan"}},
            {"nodeId": "6", "parentId": "1", "role": {"value": "paragraph"}, "childIds": ["7", "8", "9"]},
            {"nodeId": "7", "parentId": "6", "role": {"value": "StaticText"}, "name": {"value": "Read the"}},
            {"nodeId": "8", "parentId": "6", "role": {"value": "strong"}, "childIds": ["10"]},
            {"nodeId": "10", "parentId": "8", "role": {"value": "StaticText"}, "name": {"value": "terms"}},
            {"nodeId": "9", "parentId": "6", "role": {"value": "StaticText"}, "name": {"value": "first."}}
        ]));
        let mut extra = HashMap::new();
        extra.insert(20, Extra { kind: "clickable".into(), label: "Open card".into(), checked: None });
        extra.insert(21, Extra { kind: "radio".into(), label: "Pro plan".into(), checked: Some(true) });
        let lines = render_with(&t, SnapshotOptions { interactive: true, ..Default::default() }, &extra);
        let refs: Vec<_> = (1..=lines.len()).map(|i| Some(format!("e{i}"))).collect();
        assert_eq!(format_lines(&lines, &refs, &HashMap::new()), "- clickable \"Open card\" [ref=e1] [no role]\n- radio \"Pro plan\" [ref=e2] [checked]\n");
        let lines = merge_text(render_with(&t, SnapshotOptions::default(), &extra));
        let text = format_lines(&lines, &vec![None; lines.len()], &HashMap::new());
        assert!(text.contains("- text: Read the terms first.\n"), "{text}");
        // The found control's own text isn't repeated under it
        assert!(!text.contains("text: Open card"), "{text}");
    }

    #[test]
    fn interactive_snapshot_lists_controls_only() {
        let t = sample();
        let lines = render(&t, SnapshotOptions { interactive: true, ..Default::default() });
        let refs: Vec<_> = (1..=lines.len()).map(|i| Some(format!("e{i}"))).collect();
        let text = format_lines(&lines, &refs, &HashMap::new());
        assert_eq!(text, "- link \"Home\" [ref=e1]\n- textbox \"User\" [ref=e2] value=\"ann\"\n- checkbox \"Remember\" [ref=e3] [checked]\n");
    }

    #[test]
    fn full_snapshot_keeps_text_and_skips_duplicate_link_text() {
        let t = sample();
        let lines = render(&t, SnapshotOptions::default());
        let text = format_lines(&lines, &vec![None; lines.len()], &HashMap::new());
        assert!(text.contains("- navigation\n  - link \"Home\""), "{text}");
        assert!(!text.contains("text: Home"), "{text}");
        assert!(text.contains("- text: Forgot password?"), "{text}");
        // The root web area and the generic div print nothing themselves
        assert!(!text.contains("RootWebArea") && !text.contains("generic"), "{text}");
    }

    #[test]
    fn compact_and_depth() {
        let t = tree(json!([
            {"nodeId": "1", "role": {"value": "RootWebArea"}, "childIds": ["2", "3"]},
            {"nodeId": "2", "parentId": "1", "role": {"value": "navigation"}, "childIds": []},
            {"nodeId": "3", "parentId": "1", "role": {"value": "main"}, "childIds": ["4"]},
            {"nodeId": "4", "parentId": "3", "role": {"value": "button"}, "name": {"value": "Go"}, "backendDOMNodeId": 5}
        ]));
        let lines = render(&t, SnapshotOptions { compact: true, ..Default::default() });
        assert_eq!(lines.iter().map(|l| l.role.as_str()).collect::<Vec<_>>(), ["main", "button"]);
        let lines = render(&t, SnapshotOptions { depth: Some(0), ..Default::default() });
        assert_eq!(lines.iter().map(|l| l.role.as_str()).collect::<Vec<_>>(), ["navigation", "main"]);
    }

    #[test]
    fn ignored_nodes_pass_their_children_through() {
        let t = tree(json!([
            {"nodeId": "1", "role": {"value": "RootWebArea"}, "childIds": ["2"]},
            {"nodeId": "2", "parentId": "1", "ignored": true, "role": {"value": "none"}, "childIds": ["3"]},
            {"nodeId": "3", "parentId": "2", "role": {"value": "button"}, "name": {"value": "\u{200B}Save "}, "backendDOMNodeId": 9}
        ]));
        let lines = render(&t, SnapshotOptions { interactive: true, ..Default::default() });
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].name, "Save");
        assert_eq!(lines[0].target, Some(("s".into(), "f".into(), 9)));
    }
}
