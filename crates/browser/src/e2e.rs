//! Against a real Chrome / Edge: `YI_BROWSER_E2E=1 cargo test -p yi-browser e2e -- --nocapture`
//! (`YI_BROWSER_EXE` picks the browser, `YI_BROWSER_HEADED=1` shows the window). Skipped otherwise.

use crate::{find_browser, Browser, Hooks, LaunchOptions};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const INDEX: &str = r##"<!doctype html><html><head><title>Fixture</title>
<style>#cover{position:fixed;left:0;top:300px;width:400px;height:60px;background:#ccc}#under{position:fixed;left:10px;top:310px}
.vh{position:absolute;opacity:0;width:1px;height:1px}</style></head><body>
<h1>Sign in</h1>
<label>User <input id="user" name="user"></label>
<input type="checkbox" id="rem" class="vh"><label for="rem">Remember me</label>
<select id="color" aria-label="Color"><option value="r">Red</option><option value="g">Green</option></select>
<button id="login" onclick="if(confirm('Really sign in?')){document.getElementById('out').textContent='confirmed '+document.getElementById('user').value}">Login</button>
<button onclick="alert('hello')">Alert</button>
<div id="out">idle</div>
<a href="/next">Next page</a>
<button id="under">Hidden action</button><div id="cover">banner</div>
<button aria-label="" class="lucide-trash"><svg><use href="#icon-trash"></use></svg></button>
<iframe src="/frame" title="inner" style="position:absolute;left:500px;top:20px;width:300px;height:150px"></iframe>
<script>console.log('fixture ready', 42); setTimeout(()=>{throw new Error('boom')},0)</script>
</body></html>"##;

const FRAME: &str = r#"<!doctype html><html><body><button onclick="this.textContent='pressed'">Frame button</button></body></html>"#;
const NEXT: &str = r#"<!doctype html><html><head><title>Next</title></head><body><h2>Second</h2></body></html>"#;

async fn serve() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { break };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/");
                let body = match path {
                    "/frame" => FRAME,
                    "/next" => NEXT,
                    _ => INDEX,
                };
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    port
}

fn find_ref(snapshot: &str, needle: &str) -> String {
    let line = snapshot.lines().find(|l| l.contains(needle)).unwrap_or_else(|| panic!("no {needle} in\n{snapshot}"));
    let at = line.find("[ref=").unwrap() + 5;
    line[at..].split(']').next().unwrap().to_string()
}

#[tokio::test]
async fn e2e_real_browser() {
    if std::env::var("YI_BROWSER_E2E").is_err() {
        eprintln!("skipped: set YI_BROWSER_E2E=1");
        return;
    }
    let exe = find_browser(std::env::var_os("YI_BROWSER_EXE").map(std::path::PathBuf::from).as_deref()).expect("no Chrome / Edge found");
    let profile = tempfile::tempdir().unwrap();
    let opts = LaunchOptions::new(exe.clone(), profile.path().to_path_buf(), std::env::var("YI_BROWSER_HEADED").is_err());
    let none = |_: &mut tokio::process::Command| {};
    let attach = |_: &tokio::process::Child| -> Box<dyn std::any::Any + Send + Sync> { Box::new(()) };
    let b = Browser::launch(opts, Hooks { prepare: &none, attach: &attach }).await.expect("launch");
    eprintln!("browser {} ({})", b.version, exe.display());
    let port = serve().await;
    let url = format!("http://127.0.0.1:{port}/");
    let call = |t: &'static str, a: serde_json::Value| {
        let b = &b;
        async move {
            let r = b.call(t, &a).await;
            match &r {
                Ok(o) => eprintln!("== {t} {a}\n{}", o.text("N")),
                Err(e) => eprintln!("== {t} {a}\nERR {e:#}"),
            }
            r
        }
    };

    let o = call("browser_open", json!({"url": url})).await.unwrap();
    assert!(o.summary.contains("已打开") && o.summary.contains("Fixture"), "{}", o.summary);

    let snap = call("browser_snapshot", json!({})).await.unwrap().page.unwrap();
    let user = find_ref(&snap, "textbox \"User\"");
    let remember = find_ref(&snap, "checkbox \"Remember me\"");
    let color = find_ref(&snap, "combobox \"Color\"");
    let login = find_ref(&snap, "button \"Login\"");
    let hidden = find_ref(&snap, "button \"Hidden action\"");
    let next = find_ref(&snap, "link \"Next page\"");
    let frame_btn = find_ref(&snap, "button \"Frame button\"");
    assert!(snap.contains("icon:icon-trash"), "unnamed button gets a hint:\n{snap}");
    // A second snapshot keeps the refs of the same elements
    let again = call("browser_snapshot", json!({})).await.unwrap().page.unwrap();
    assert_eq!(find_ref(&again, "button \"Login\""), login);

    call("browser_fill", json!({"ref": user, "text": "张三 ann"})).await.unwrap();
    assert_eq!(call("browser_get", json!({"what": "value", "ref": user})).await.unwrap().page.unwrap(), "张三 ann");

    call("browser_check", json!({"ref": remember})).await.unwrap();
    assert_eq!(call("browser_get", json!({"what": "checked", "ref": remember})).await.unwrap().summary, "true");
    let o = call("browser_check", json!({"ref": remember, "checked": true})).await.unwrap();
    assert!(o.summary.contains("已经是"), "{}", o.summary);

    call("browser_select", json!({"ref": color, "values": ["Green"]})).await.unwrap();
    assert_eq!(call("browser_get", json!({"what": "value", "ref": color})).await.unwrap().page.unwrap(), "g");

    let e = call("browser_click", json!({"ref": hidden})).await.unwrap_err().to_string();
    assert!(e.contains("挡住") && e.contains("banner"), "{e}");

    // confirm: the click returns with the dialog pending; everything else waits for it
    let o = call("browser_click", json!({"ref": login})).await.unwrap();
    assert!(o.summary.contains("confirm") && o.summary.contains("Really sign in?"), "{}", o.summary);
    assert!(call("browser_snapshot", json!({})).await.is_err());
    call("browser_dialog", json!({"action": "accept"})).await.unwrap();
    let text = call("browser_get", json!({"what": "text"})).await.unwrap().page.unwrap();
    assert!(text.contains("confirmed 张三 ann"), "{text}");

    // alert: accepted automatically and reported
    let snap = call("browser_snapshot", json!({})).await.unwrap().page.unwrap();
    let alert = find_ref(&snap, "button \"Alert\"");
    let o = call("browser_click", json!({"ref": alert})).await.unwrap();
    let all = o.text("N");
    assert!(all.contains("alert") && all.contains("hello"), "{all}");

    let c = call("browser_console", json!({})).await.unwrap().page.unwrap();
    assert!(c.contains("fixture ready 42") && c.contains("boom"), "{c}");

    // A button inside the iframe, clicked without switching frames
    call("browser_click", json!({"ref": frame_btn})).await.unwrap();
    assert_eq!(call("browser_get", json!({"what": "text", "ref": frame_btn})).await.unwrap().page.unwrap(), "pressed");

    let o = call("browser_screenshot", json!({"annotate": true})).await.unwrap();
    let png = o.image.unwrap();
    assert_eq!(&png[..4], b"\x89PNG");
    let o = call("browser_screenshot", json!({"ifChanged": true})).await.unwrap();
    assert!(o.image.is_some());
    let o = call("browser_screenshot", json!({"ifChanged": true})).await.unwrap();
    assert!(o.image.is_none(), "unchanged page returns no image");

    let o = call("browser_click", json!({"ref": next})).await.unwrap();
    assert!(o.summary.contains("/next") && o.summary.contains("失效"), "{}", o.summary);
    let e = call("browser_click", json!({"ref": login})).await.unwrap_err().to_string();
    assert!(e.contains("已失效"), "{e}");
    let snap = call("browser_snapshot", json!({"interactive": false})).await.unwrap().page.unwrap();
    assert!(snap.contains("heading \"Second\""), "{snap}");

    let o = call("browser_open", json!({"action": "back"})).await.unwrap();
    assert!(o.summary.starts_with("已打开"), "back/forward cache restores count as loaded: {}", o.summary);
    assert_eq!(b.current_url(), url);

    let o = call("browser_tabs", json!({"action": "new", "url": format!("http://127.0.0.1:{port}/next")})).await.unwrap();
    assert!(o.summary.contains("* t2"), "{}", o.summary);
    call("browser_tabs", json!({"action": "close", "tab": "t2"})).await.unwrap();
    assert_eq!(b.tab_list().len(), 1);

    assert!(call("browser_open", json!({"url": "file:///etc/passwd"})).await.is_err());
    let w = call("browser_wait", json!({"text": "never there", "timeoutMs": 300})).await.unwrap_err().to_string();
    assert!(w.contains("超时"), "{w}");

    b.close().await;
    assert!(b.call("browser_snapshot", &json!({})).await.is_err());
}

const LIVE: &str = r##"<!doctype html><html><head><title>Live</title></head><body>
<button id="count" style="position:absolute;left:20px;top:20px;width:160px;height:40px" onclick="this.textContent='Count '+(++window.n)">Count 0</button>
<input id="typed" aria-label="Typed" style="position:absolute;left:20px;top:80px;width:200px">
<button id="fetch" style="position:absolute;left:20px;top:130px" onclick="fetch('/api/data').then(r=>r.json()).then(j=>document.getElementById('got').textContent='got '+j.v)">Fetch</button>
<div id="got" style="position:absolute;left:20px;top:170px">none</div>
<a id="dl" href="/file.csv" style="position:absolute;left:20px;top:200px">Download report</a>
<input type="file" id="up" aria-label="Upload" style="position:absolute;left:20px;top:230px" onchange="document.getElementById('picked').textContent='picked '+[...this.files].map(f=>f.name).join(',')">
<div id="picked" style="position:absolute;left:20px;top:260px">nothing</div>
<iframe src="/inner" title="inner frame" style="position:absolute;left:400px;top:20px;width:300px;height:120px"></iframe>
<script>window.n=0</script>
</body></html>"##;
const INNER: &str = r#"<!doctype html><html><body><p id="x">inside text</p><button>Inner</button></body></html>"#;
const GUARD: &str = r##"<!doctype html><html><head><title>Guard</title></head><body><div id="out">running</div><script>
const other='http://localhost:PORT', out=[];
(async()=>{
  try{ await fetch(other+'/api/data',{mode:'no-cors'}); out.push('fetch:ok') }catch(e){ out.push('fetch:blocked') }
  try{ await fetch('/api/data'); out.push('same:ok') }catch(e){ out.push('same:blocked') }
  try{ new WebSocket('ws://localhost:PORT/ws'); out.push('ws:ok') }catch(e){ out.push('ws:blocked') }
  out.push('beacon:'+navigator.sendBeacon(other+'/beacon','x'));
  out.push('rtc:'+typeof RTCPeerConnection);
  out.push('sw:'+typeof navigator.serviceWorker);
  out.push(await new Promise(res=>{ try{ const w=new Worker('/worker.js'); w.onmessage=e=>res(e.data); w.onerror=()=>res('worker:error'); setTimeout(()=>res('worker:timeout'),5000) }catch(e){ res('worker:throw') } }));
  document.getElementById('out').textContent=out.join(' ');
})();
</script></body></html>"##;

async fn serve_live() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { break };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/");
                let (ctype, extra, body) = match path {
                    "/inner" => ("text/html; charset=utf-8", "", INNER.to_string()),
                    "/guard" => ("text/html; charset=utf-8", "", GUARD.replace("PORT", &port.to_string())),
                    "/api/data" => ("application/json", "", r#"{"v":"real"}"#.to_string()),
                    "/worker.js" => ("text/javascript", "", format!("fetch('http://localhost:{port}/api/data',{{mode:'no-cors'}}).then(()=>postMessage('wfetch:ok'),()=>postMessage('wfetch:blocked'))")),
                    "/file.csv" => ("text/csv", "Content-Disposition: attachment; filename=\"report.csv\"\r\n", "a,b\n1,2\n".to_string()),
                    _ => ("text/html; charset=utf-8", "", LIVE.to_string()),
                };
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    port
}

/// Live view, human input, network log and routes, frames, diff, emulation, eval, downloads, uploads, allowlist
#[tokio::test]
async fn e2e_p1b_real_browser() {
    if std::env::var("YI_BROWSER_E2E").is_err() {
        eprintln!("skipped: set YI_BROWSER_E2E=1");
        return;
    }
    let exe = find_browser(std::env::var_os("YI_BROWSER_EXE").map(std::path::PathBuf::from).as_deref()).expect("no Chrome / Edge found");
    let profile = tempfile::tempdir().unwrap();
    let downloads = tempfile::tempdir().unwrap();
    let mut opts = LaunchOptions::new(exe.clone(), profile.path().to_path_buf(), true);
    opts.download_dir = Some(downloads.path().to_path_buf());
    let none = |_: &mut tokio::process::Command| {};
    let attach = |_: &tokio::process::Child| -> Box<dyn std::any::Any + Send + Sync> { Box::new(()) };
    let b = Browser::launch(opts, Hooks { prepare: &none, attach: &attach }).await.expect("launch");
    let port = serve_live().await;
    let base = format!("http://127.0.0.1:{port}");
    let call = |t: &'static str, a: serde_json::Value| {
        let b = &b;
        async move {
            let r = b.call(t, &a).await;
            match &r {
                Ok(o) => eprintln!("== {t} {a}\n{}", o.text("N")),
                Err(e) => eprintln!("== {t} {a}\nERR {e:#}"),
            }
            r
        }
    };
    call("browser_open", json!({"url": format!("{base}/")})).await.unwrap();

    // Screencast: a JPEG of the viewport arrives, with the viewport size in CSS pixels
    let mut frames = b.frames();
    b.set_screencast(Some(crate::CastConfig { max_width: 800, max_height: 600, quality: 50 }));
    let frame = tokio::time::timeout(std::time::Duration::from_secs(10), frames.wait_for(|f| f.is_some())).await.expect("no frame").unwrap().clone().unwrap();
    assert!(frame.data.starts_with("/9j/"), "jpeg base64");
    let vw = frame.metadata["deviceWidth"].as_f64().unwrap();
    assert!(vw > 600.0, "{}", frame.metadata);
    // The title follows Target.targetInfoChanged, a moment after the load
    for _ in 0..30 {
        if b.view_state().title == "Live" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let st = b.view_state();
    assert!(st.url.ends_with(&format!("{port}/")) && st.title == "Live" && st.tabs.len() == 1, "{st:?}");

    // A person clicks the button and types Chinese into the field, without the tool lock
    for ev in ["down", "up"] {
        b.human_input(&json!({"type": "mouse", "event": ev, "x": 100, "y": 40, "button": "left", "buttons": if ev == "down" { 1 } else { 0 }, "clickCount": 1})).unwrap();
    }
    for ev in ["down", "up"] {
        b.human_input(&json!({"type": "mouse", "event": ev, "x": 100, "y": 90, "button": "left", "clickCount": 1})).unwrap();
    }
    b.human_input(&json!({"type": "text", "text": "你好"})).unwrap();
    b.human_input(&json!({"type": "key", "event": "down", "key": "a", "code": "KeyA", "keyCode": 65, "text": "a"})).unwrap();
    b.human_input(&json!({"type": "key", "event": "up", "key": "a", "code": "KeyA", "keyCode": 65})).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // A new frame shows the change
    assert!(tokio::time::timeout(std::time::Duration::from_secs(5), frames.changed()).await.is_ok());
    let o = call("browser_eval", json!({"expression": "[document.getElementById('count').textContent, document.getElementById('typed').value]"})).await.unwrap();
    assert!(o.page.as_deref().unwrap().contains("Count 1") && o.page.as_deref().unwrap().contains("你好a"), "{:?}", o.page);
    assert!(o.notes.iter().any(|n| n.contains("用户在实时画面里操作过")), "{:?}", o.notes);
    let o = call("browser_get", json!({"what": "title"})).await.unwrap();
    assert!(o.notes.is_empty(), "the note comes once");
    b.set_screencast(None);

    // Network log and a route
    let snap = call("browser_snapshot", json!({})).await.unwrap().page.unwrap();
    let fetch_btn = find_ref(&snap, "button \"Fetch\"");
    call("browser_click", json!({"ref": fetch_btn})).await.unwrap();
    // The live view knows where the AI clicked: the Fetch button sits at left 20, top 130
    let p = b.view_state().pointer.expect("pointer after a click");
    assert!(p.clicks >= 1 && p.x > 20.0 && p.x < 120.0 && p.y > 125.0 && p.y < 160.0, "{p:?}");
    call("browser_wait", json!({"text": "got real"})).await.unwrap();
    let net = call("browser_network", json!({"filter": "api/data"})).await.unwrap().page.unwrap();
    assert!(net.contains("/api/data 200 [Fetch]"), "{net}");
    call("browser_network", json!({"action": "route", "url": "*/api/data", "body": "{\"v\":\"mock\"}"})).await.unwrap();
    call("browser_click", json!({"ref": fetch_btn})).await.unwrap();
    call("browser_wait", json!({"text": "got mock"})).await.unwrap();
    call("browser_network", json!({"action": "unroute"})).await.unwrap();

    // Into the iframe and back
    let full = call("browser_snapshot", json!({"interactive": true})).await.unwrap().page.unwrap();
    let iframe = find_ref(&full, "Iframe");
    let o = call("browser_frame", json!({"ref": iframe})).await.unwrap();
    assert!(o.summary.contains("/inner"), "{}", o.summary);
    let t = call("browser_get", json!({"what": "text"})).await.unwrap().page.unwrap();
    assert!(t.contains("inside text") && !t.contains("Count"), "{t}");
    let snap = call("browser_snapshot", json!({})).await.unwrap().page.unwrap();
    assert!(snap.contains("button \"Inner\"") && !snap.contains("Fetch"), "{snap}");
    assert_eq!(b.view_state().frame.is_some(), true);
    call("browser_frame", json!({})).await.unwrap();
    assert!(call("browser_get", json!({"what": "text"})).await.unwrap().page.unwrap().contains("Count 1"));

    // Diff against the previous snapshot
    call("browser_snapshot", json!({})).await.unwrap();
    let count = find_ref(&snap_of(&b).await, "button \"Count 1\"");
    call("browser_click", json!({"ref": count})).await.unwrap();
    let o = call("browser_diff", json!({})).await.unwrap();
    let d = o.page.unwrap_or_default();
    assert!(d.contains("- - button \"Count 1\"") && d.contains("+ - button \"Count 2\""), "{d}");
    assert!(call("browser_diff", json!({})).await.unwrap().summary.contains("没有变化"));

    // Emulation
    call("browser_set", json!({"colorScheme": "dark", "width": 800, "height": 600})).await.unwrap();
    let v = call("browser_eval", json!({"expression": "({dark: matchMedia('(prefers-color-scheme: dark)').matches, w: innerWidth})"})).await.unwrap().page.unwrap();
    assert!(v.contains("\"dark\": true") && v.contains("\"w\": 800"), "{v}");
    call("browser_set", json!({"reset": true})).await.unwrap();
    let v = call("browser_eval", json!({"expression": "innerWidth"})).await.unwrap().page.unwrap();
    assert_ne!(v, "800");
    assert!(call("browser_eval", json!({"expression": "throw new Error('nope')"})).await.unwrap_err().to_string().contains("nope"));

    // Download and upload
    let snap = call("browser_snapshot", json!({})).await.unwrap().page.unwrap();
    let dl = find_ref(&snap, "link \"Download report\"");
    let o = call("browser_download", json!({"ref": dl})).await.unwrap();
    let saved = downloads.path().join("report.csv");
    assert!(o.summary.contains("report.csv"), "{}", o.summary);
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), "a,b\n1,2\n");
    let up = find_ref(&snap, "Upload");
    let cwd = tempfile::tempdir().unwrap();
    std::fs::write(cwd.path().join("notes.txt"), "hi").unwrap();
    let roots = crate::UploadRoots { cwd: Some(cwd.path().into()), roots: vec![cwd.path().into(), downloads.path().into()] };
    b.call_with("browser_upload", &json!({"ref": up, "files": ["notes.txt", saved.to_string_lossy()]}), &roots).await.unwrap_err();
    b.call_with("browser_upload", &json!({"ref": up, "files": ["notes.txt"]}), &roots).await.unwrap();
    call("browser_wait", json!({"text": "picked notes.txt"})).await.unwrap();
    assert!(b.call_with("browser_upload", &json!({"ref": up, "files": [profile.path().join("Local State").to_string_lossy()]}), &roots).await.is_err());
    b.close().await;

    // Allowlist: other hosts are blocked for requests, WebSocket, sendBeacon; WebRTC is gone
    let profile2 = tempfile::tempdir().unwrap();
    let mut opts = LaunchOptions::new(exe, profile2.path().to_path_buf(), true);
    opts.allowed_domains = vec!["127.0.0.1".into()];
    let g = Browser::launch(opts, Hooks { prepare: &none, attach: &attach }).await.expect("launch");
    assert!(g.call("browser_open", &json!({"url": format!("http://localhost:{port}/")})).await.unwrap_err().to_string().contains("允许"));
    let mut notes = g.call("browser_open", &json!({"url": format!("{base}/guard")})).await.unwrap().notes;
    notes.extend(g.call("browser_wait", &json!({"text": "rtc:", "timeoutMs": 20000})).await.unwrap().notes);
    let o = g.call("browser_get", &json!({"what": "text"})).await.unwrap();
    notes.extend(o.notes);
    let t = o.page.unwrap();
    eprintln!("guard: {t} {notes:?}");
    assert!(
        t.contains("fetch:blocked") && t.contains("same:ok") && t.contains("ws:blocked") && t.contains("beacon:false") && t.contains("rtc:undefined") && t.contains("sw:undefined") && t.contains("wfetch:blocked"),
        "{t}"
    );
    // The model hears which origin was blocked, once
    assert_eq!(notes.iter().filter(|n| n.contains("localhost")).count(), 1, "{notes:?}");
    g.close().await;
}

async fn snap_of(b: &Browser) -> String {
    b.call("browser_snapshot", &json!({})).await.unwrap().page.unwrap()
}

const BORROW: &str = r##"<!doctype html><html><head><title>Borrow</title>
<style>.card{cursor:pointer;padding:8px;border:1px solid #888;width:200px}.plan input{display:none}.plan{display:inline-block;padding:6px;border:1px solid #aaa}
#lb{position:absolute;left:300px;top:40px;background:#fff;border:1px solid #333}#pane{height:80px;overflow:auto;width:200px;border:1px solid #000}
#src,#dst{display:inline-block;width:120px;height:40px;border:1px solid #333;margin:4px}</style></head><body>
<div class="card" id="card" onclick="this.textContent='card opened'">Open card</div>
<label class="plan"><input type="radio" name="plan" value="free">Free plan</label><label class="plan"><input type="radio" name="plan" value="pro">Pro plan</label>
<button data-testid="save" onclick="document.getElementById('out').textContent='saved'">Save</button>
<button id="mods" onclick="document.getElementById('out').textContent='shift '+event.shiftKey" onauxclick="document.getElementById('out').textContent='aux '+event.button">Mods</button>
<div id="cb" role="combobox" aria-expanded="false" tabindex="0" style="display:inline-block;border:1px solid #333;padding:4px" onclick="const l=document.getElementById('lb');l.hidden=!l.hidden;this.setAttribute('aria-expanded',String(!l.hidden))">Pick fruit</div>
<div id="lb" role="listbox" hidden><div role="option" onclick="pick(this)">Apple</div><div role="option" onclick="pick(this)">Banana&nbsp;split</div></div>
<div><div id="src" draggable="true" ondragstart="event.dataTransfer.setData('text/plain','card-7')">Drag me</div><div id="dst" ondragover="event.preventDefault()" ondrop="event.preventDefault();this.textContent='dropped '+event.dataTransfer.getData('text/plain')">Drop here</div></div>
<div id="pane" aria-label="Pane"><div style="height:1000px">long content</div></div>
<div id="spin">Loading…</div>
<div id="out">idle</div>
<script>
function pick(o){ const c=document.getElementById('cb'); c.textContent=o.textContent; document.getElementById('lb').hidden=true; c.setAttribute('aria-expanded','false'); }
setTimeout(()=>document.getElementById('spin').remove(), 600);
setTimeout(()=>{ window.ready=true }, 300);
</script></body></html>"##;

const API: &str = r##"<!doctype html><html><head><title>Api</title></head><body><div id="out">idle</div><script>
async function post(){ const r=await fetch('/api/echo',{method:'POST',headers:{'Content-Type':'application/json','Authorization':'Bearer topsecret'},body:JSON.stringify({name:'ann'})}); document.getElementById('out').textContent=await r.text(); }
async function cors(){ try{ const r=await fetch('http://localhost:PORT/api/cors',{method:'POST',credentials:'include',headers:{'Content-Type':'application/json','X-Token':'1'},body:'{}'}); document.getElementById('out').textContent='cors '+(await r.json()).v; }catch(e){ document.getElementById('out').textContent='cors failed' } }
async function hdr(){ const r=await fetch('/api/headers'); document.getElementById('out').textContent='hdr '+await r.text(); }
document.title='bad \ud800 title'; console.log('lone \udc00 surrogate');
</script></body></html>"##;

async fn serve_borrow() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { break };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let header = |name: &str| req.lines().find(|l| l.to_ascii_lowercase().starts_with(&format!("{name}:"))).map(|l| l.split_once(':').unwrap().1.trim().to_string());
                let html = "text/html; charset=utf-8";
                let (code, ctype, extra, body): (u16, &str, String, String) = match path.as_str() {
                    "/" => (200, html, "Set-Cookie: sid=abc123; HttpOnly; Path=/\r\n".into(), BORROW.to_string()),
                    "/api" => (200, html, String::new(), API.replace("PORT", &port.to_string())),
                    "/api/echo" => (200, "application/json", String::new(), format!("{{\"echo\":{}}}", req.split("\r\n\r\n").nth(1).unwrap_or("null"))),
                    "/api/headers" => (200, "text/plain", String::new(), header("x-test").unwrap_or_else(|| "none".into())),
                    "/missing" => (404, html, String::new(), "<title>Not here</title><h1>404</h1>".into()),
                    "/errors" => (200, html, String::new(), "<title>Errs</title><script>console.error('bad thing');throw new Error('broken')</script>".into()),
                    "/secret" => match header("authorization") {
                        Some(a) if a == "Basic dTpw" => (200, html, String::new(), "<title>Secret</title>secret ok".into()),
                        _ => (401, html, "WWW-Authenticate: Basic realm=\"test\"\r\n".into(), "denied".into()),
                    },
                    _ => (404, "text/plain", String::new(), "no".into()),
                };
                let resp = format!("HTTP/1.1 {code} X\r\nContent-Type: {ctype}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    port
}

/// What came from agent-browser: found controls, selectors, find, custom select, drag, modifiers, container scroll,
/// waits, request detail, route CORS, cookies, storage, headers, basic auth, devices, element JPEG, hints, tab gone
#[tokio::test]
async fn e2e_borrowed_real_browser() {
    if std::env::var("YI_BROWSER_E2E").is_err() {
        eprintln!("skipped: set YI_BROWSER_E2E=1");
        return;
    }
    let exe = find_browser(std::env::var_os("YI_BROWSER_EXE").map(std::path::PathBuf::from).as_deref()).expect("no Chrome / Edge found");
    let profile = tempfile::tempdir().unwrap();
    let opts = LaunchOptions::new(exe, profile.path().to_path_buf(), true);
    let none = |_: &mut tokio::process::Command| {};
    let attach = |_: &tokio::process::Child| -> Box<dyn std::any::Any + Send + Sync> { Box::new(()) };
    let b = Browser::launch(opts, Hooks { prepare: &none, attach: &attach }).await.expect("launch");
    let port = serve_borrow().await;
    let base = format!("http://127.0.0.1:{port}");
    let call = |t: &'static str, a: serde_json::Value| {
        let b = &b;
        async move {
            let r = b.call(t, &a).await;
            match &r {
                Ok(o) => eprintln!("== {t} {a}\n{}", o.text("N")),
                Err(e) => eprintln!("== {t} {a}\nERR {e:#}"),
            }
            r
        }
    };
    let out = || async { call("browser_get", json!({"what": "text", "selector": "#out"})).await.unwrap().page.unwrap() };
    call("browser_open", json!({"url": format!("{base}/")})).await.unwrap();

    // Controls without a role, and labels standing for hidden radios, get refs
    let snap = call("browser_snapshot", json!({})).await.unwrap().page.unwrap();
    let card = find_ref(&snap, "clickable \"Open card\"");
    let pro = find_ref(&snap, "radio \"Pro plan\"");
    call("browser_click", json!({"ref": card})).await.unwrap();
    assert_eq!(call("browser_get", json!({"what": "text", "selector": "#card"})).await.unwrap().page.unwrap(), "card opened");
    call("browser_check", json!({"ref": pro})).await.unwrap();
    assert_eq!(call("browser_eval", json!({"expression": "document.querySelector('input[value=pro]').checked"})).await.unwrap().page.unwrap(), "true");

    // Selectors and find
    call("browser_click", json!({"selector": "[data-testid=save]"})).await.unwrap();
    assert_eq!(out().await, "saved");
    let f = call("browser_find", json!({"by": "testid", "value": "save"})).await.unwrap().page.unwrap();
    assert!(f.contains("button \"Save\" [ref="), "{f}");
    let f = call("browser_find", json!({"by": "role", "value": "button", "name": "mods"})).await.unwrap().page.unwrap();
    let mods = find_ref(&f, "Mods");
    let e = call("browser_click", json!({"selector": "#nothing-here"})).await.unwrap_err().to_string();
    assert!(e.contains("没有元素匹配"), "{e}");
    assert!(call("browser_get", json!({"what": "count", "selector": "label.plan"})).await.unwrap().summary.starts_with("2 "));
    assert_eq!(call("browser_get", json!({"what": "attr", "selector": "#cb", "name": "role"})).await.unwrap().page.unwrap(), "combobox");
    assert!(call("browser_get", json!({"what": "box", "ref": card})).await.unwrap().summary.contains("\"width\": 2"));

    // Modifier and middle clicks
    call("browser_click", json!({"ref": mods, "modifiers": ["Shift"]})).await.unwrap();
    assert_eq!(out().await, "shift true");
    call("browser_click", json!({"ref": mods, "kind": "middleclick"})).await.unwrap();
    assert_eq!(out().await, "aux 1");

    // A custom combobox: opened, option matched with a non-breaking space in its label
    call("browser_select", json!({"selector": "#cb", "values": ["Banana split"]})).await.unwrap();
    assert_eq!(call("browser_get", json!({"what": "text", "selector": "#cb"})).await.unwrap().page.unwrap().replace('\u{a0}', " "), "Banana split");

    // HTML5 drag and drop
    call("browser_drag", json!({"from": "#src", "to": "#dst"})).await.unwrap();
    assert_eq!(call("browser_get", json!({"what": "text", "selector": "#dst"})).await.unwrap().page.unwrap(), "dropped card-7");

    // Scrolling inside a pane
    let o = call("browser_scroll", json!({"selector": "#pane", "direction": "down", "amount": 300})).await.unwrap();
    assert!(o.summary.contains("当前位置 300"), "{}", o.summary);

    // Waits for something to go away and for a script condition
    call("browser_wait", json!({"selector": "#spin", "state": "hidden", "timeoutMs": 5000})).await.unwrap();
    call("browser_wait", json!({"fn": "window.ready === true"})).await.unwrap();
    let e = call("browser_wait", json!({"text": "never", "timeoutMs": 300})).await.unwrap_err().to_string();
    assert!(e.contains("文本「never」出现") && e.contains("browser_snapshot"), "{e}");

    // Element screenshot as JPEG; snapshot of one ref; diff with other options
    let o = call("browser_screenshot", json!({"ref": card, "format": "jpeg", "quality": 60})).await.unwrap();
    assert_eq!(o.image_mime, "image/jpeg");
    assert_eq!(&o.image.unwrap()[..2], b"\xff\xd8");
    let part = call("browser_snapshot", json!({"ref": card, "interactive": false})).await.unwrap().page.unwrap();
    assert!(part.contains("card opened") && !part.contains("Save"), "{part}");
    assert!(call("browser_diff", json!({})).await.unwrap().summary.contains("选项不同"));

    // Cookies: the httpOnly one the server set is visible; one set here reaches the page
    let c = call("browser_cookies", json!({})).await.unwrap().page.unwrap();
    assert!(c.contains("sid=abc123") && c.contains("httpOnly"), "{c}");
    call("browser_cookies", json!({"action": "set", "name": "flag", "value": "on"})).await.unwrap();
    assert!(call("browser_eval", json!({"expression": "document.cookie"})).await.unwrap().page.unwrap().contains("flag=on"));
    call("browser_storage", json!({"action": "set", "key": "feature", "value": "beta"})).await.unwrap();
    assert_eq!(call("browser_storage", json!({"key": "feature"})).await.unwrap().page.unwrap(), "beta");
    assert!(call("browser_storage", json!({})).await.unwrap().page.unwrap().contains("\"feature\": \"beta\""));

    // A page with lone surrogates in its title and console doesn't hang anything
    call("browser_open", json!({"url": format!("{base}/api")})).await.unwrap();
    let t0 = std::time::Instant::now();
    call("browser_console", json!({})).await.unwrap();
    call("browser_snapshot", json!({})).await.unwrap();
    assert!(t0.elapsed() < std::time::Duration::from_secs(10));

    // Request detail: body, response, secrets hidden
    call("browser_eval", json!({"expression": "post()"})).await.unwrap();
    call("browser_wait", json!({"text": "echo"})).await.unwrap();
    let list = call("browser_network", json!({"method": "POST", "code": "2xx"})).await.unwrap().page.unwrap();
    let id = list.lines().find(|l| l.contains("/api/echo")).and_then(|l| l.split_whitespace().next()).unwrap().trim_start_matches('#').to_string();
    let d = call("browser_network", json!({"action": "detail", "id": id})).await.unwrap().page.unwrap();
    assert!(d.contains("{\"name\":\"ann\"}") && d.contains("<已隐藏") && !d.contains("topsecret") && d.contains("\"echo\""), "{d}");

    // A mocked cross-origin POST with credentials and a custom header: preflight answered, CORS passes
    call("browser_network", json!({"action": "route", "url": "*/api/cors", "body": "{\"v\":\"mock\"}", "headers": {"X-From": "route"}})).await.unwrap();
    call("browser_eval", json!({"expression": "cors()"})).await.unwrap();
    call("browser_wait", json!({"text": "cors ", "timeoutMs": 5000})).await.unwrap();
    assert_eq!(out().await, "cors mock");
    call("browser_network", json!({"action": "unroute"})).await.unwrap();

    // Headers for one origin only
    call("browser_set", json!({"headers": {"X-Test": "yes"}, "headersOrigin": base})).await.unwrap();
    call("browser_eval", json!({"expression": "hdr()"})).await.unwrap();
    call("browser_wait", json!({"text": "hdr "})).await.unwrap();
    assert_eq!(out().await, "hdr yes");

    // HTTP basic auth: the model is told, then answers with credentials
    let told = match call("browser_open", json!({"url": format!("{base}/secret")})).await {
        Ok(o) => o.notes.iter().any(|n| n.contains("HTTP 登录")),
        Err(e) => e.to_string().contains("HTTP 登录"),
    };
    assert!(told);
    call("browser_set", json!({"credentials": {"username": "u", "password": "p"}})).await.unwrap();
    call("browser_open", json!({"url": format!("{base}/secret")})).await.unwrap();
    assert!(call("browser_get", json!({"what": "text"})).await.unwrap().page.unwrap().contains("secret ok"));

    // Devices, timezone, geolocation
    call("browser_set", json!({"device": "iPhone 15", "timezone": "Asia/Tokyo", "locale": "ja-JP", "geolocation": {"latitude": 35.6, "longitude": 139.7}})).await.unwrap();
    let v = call("browser_eval", json!({"expression": "(async()=>({w: screen.width, dpr: devicePixelRatio, ua: navigator.userAgent.includes('iPhone'), touch: navigator.maxTouchPoints > 0, tz: Intl.DateTimeFormat().resolvedOptions().timeZone, lat: await new Promise(r=>navigator.geolocation.getCurrentPosition(p=>r(p.coords.latitude), e=>r(e.message)))}))()"})).await.unwrap().page.unwrap();
    assert!(v.contains("\"w\": 393") && v.contains("\"dpr\": 3") && v.contains("\"ua\": true") && v.contains("\"touch\": true") && v.contains("Asia/Tokyo") && v.contains("35.6"), "{v}");
    call("browser_set", json!({"reset": true})).await.unwrap();
    let v = call("browser_eval", json!({"expression": "[innerWidth, navigator.userAgent.includes('iPhone')]"})).await.unwrap().page.unwrap();
    assert!(!v.contains("393") && v.contains("false"), "{v}");

    // Navigation hints: nothing listening, an error page, console errors
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let e = call("browser_open", json!({"url": format!("http://127.0.0.1:{free}/")})).await.unwrap_err().to_string();
    assert!(e.contains("没有服务在监听"), "{e}");
    let o = call("browser_open", json!({"url": format!("{base}/missing")})).await.unwrap();
    assert!(o.summary.contains("HTTP 404"), "{}", o.summary);
    let o = call("browser_open", json!({"url": format!("{base}/errors")})).await.unwrap();
    assert!(o.summary.contains("控制台错误"), "{}", o.summary);

    // The current tab closed by someone else: the next call says so instead of acting on another tab
    call("browser_tabs", json!({"action": "new", "url": format!("{base}/missing")})).await.unwrap();
    let t2 = b.state().current.clone().unwrap();
    b.cdp().call(None, "Target.closeTarget", json!({"targetId": t2})).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let e = call("browser_snapshot", json!({})).await.unwrap_err().to_string();
    assert!(e.contains("已被关闭") && e.contains("t1"), "{e}");
    call("browser_tabs", json!({"action": "select", "tab": "t1"})).await.unwrap();
    call("browser_snapshot", json!({})).await.unwrap();

    // Nothing left waiting in the CDP client
    assert!(b.cdp().pending_count() < 3, "{}", b.cdp().pending_count());
    b.close().await;
}
