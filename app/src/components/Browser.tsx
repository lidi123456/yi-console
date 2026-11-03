import { useEffect, useRef, useState } from 'react';
import { getT } from '../i18n';
import { toast, useStore } from '../store';
import { hubCall, isTauri, rpc } from '../transport';
import { writeClipboard } from '../clipboard';
import { Icon } from './Icon';
import { forwardPort, openUrlExternal } from './PortsView';

const tx = getT('browser');

// Browser tab (iframe) in the file panel.
// - localhost addresses on a remote host are first port-forwarded to this machine, then opened in the iframe;
// - HTML files on the host are fetched by directory via the ahfs protocol (/fsv/ in yi-web), so relative resources load too;
// - addresses on the app's own origin are never loaded, and HTML previews go into a sandbox without allow-same-origin, so pages can't touch the app's APIs.
// After navigating via a link in a cross-origin page we can't read the new URL, so the address bar doesn't follow; sites that forbid embedding show blank, and we offer "Open in system browser".

/** Tab key: web:<url>, an empty url is a new tab; file:<absolute path> is an HTML file on the host */
export const WEB_PREFIX = 'web:';
export const isWebTab = (t: string) => t.startsWith(WEB_PREFIX);
export const webTabUrl = (t: string) => t.slice(WEB_PREFIX.length);
export const webTabFile = (t: string) => (webTabUrl(t).startsWith('file:') ? webTabUrl(t).slice(5) : null);

/** Name shown on the tab */
export function webTabTitle(t: string): string {
  const u = webTabUrl(t);
  if (!u) return tx('newTab');
  const f = webTabFile(t);
  if (f) return f.slice(Math.max(f.lastIndexOf('/'), f.lastIndexOf('\\')) + 1);
  try {
    const x = new URL(u);
    return x.host + (x.pathname !== '/' ? x.pathname : '');
  } catch {
    return u;
  }
}

/** User input -> full URL: 3000, localhost:3000 and example.com all work */
export function normalizeUrl(s: string): string {
  const v = s.trim();
  if (!v) return '';
  if (/^\d{1,5}(\/.*)?$/.test(v)) return `http://localhost:${v}`;
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(v)) return v;
  if (/^(localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1\])(:\d+)?(\/.*)?$/i.test(v)) return `http://${v}`;
  return `https://${v}`;
}

const LOOPBACK = new Set(['localhost', '127.0.0.1', '0.0.0.0', '[::1]', '::1']);

const tokens = new Map<string, Promise<string>>();
/** Preview URL for a file in a directory on the host */
export async function previewUrl(hostId: string, root: string, abs: string): Promise<string> {
  const k = `${hostId}\n${root}`;
  let t = tokens.get(k);
  if (!t) {
    t = hubCall<{ token: string }>('preview.root', { id: hostId, root }).then((r) => r.token);
    tokens.set(k, t);
    t.catch(() => tokens.delete(k));
  }
  const token = await t;
  const norm = (p: string) => p.replace(/\\/g, '/').replace(/\/+$/, '');
  const r = norm(root);
  const a = norm(abs);
  if (a !== r && !a.toLowerCase().startsWith(r.toLowerCase() + '/')) throw new Error(tx('onlyFilesInsideTheWorkspace'));
  const rel = a.slice(r.length + 1).split('/').map(encodeURIComponent).join('/');
  let base = `${location.origin}/fsv/`;
  if (isTauri) {
    const { convertFileSrc } = await import('@tauri-apps/api/core');
    base = convertFileSrc('', 'ahfs').replace(/\/?$/, '/');
  }
  return `${base}${token}/${rel}`;
}

type Resolved = { src: string; sandboxed: boolean; via?: string } | { error: string };

export function BrowserPane({
  hostId,
  root,
  tab,
  reload,
  onNavigate,
}: {
  hostId: string;
  root: string;
  /** Tab key (web:…) */
  tab: string;
  /** Reload when this changes (e.g. after saving an HTML file) */
  reload?: number;
  /** The address bar changed the URL: switch to the new tab key */
  onNavigate: (tab: string) => void;
}) {
  const host = useStore((s) => s.hosts.find((h) => h.config.id === hostId));
  const isLocal = host?.config.kind === 'local';
  const url = webTabUrl(tab);
  const file = webTabFile(tab);
  const [input, setInput] = useState(file ?? url);
  const [res, setRes] = useState<Resolved | null>(null);
  const [n, setN] = useState(0);
  const [loading, setLoading] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => setInput(file ?? url), [tab]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    let alive = true;
    setRes(null);
    if (!url) {
      inputRef.current?.focus();
      return;
    }
    (async (): Promise<Resolved> => {
      if (file) return { src: await previewUrl(hostId, root, file), sandboxed: true };
      let u: URL;
      try {
        u = new URL(url);
      } catch {
        return { error: tx('invalidAddress') };
      }
      if (!/^https?:$/.test(u.protocol)) return { error: tx('onlyHttpAndHttpsAddresses') };
      // localhost on a remote host is that machine itself, so forward it to this machine first
      if (!isLocal && LOOPBACK.has(u.hostname)) {
        const port = Number(u.port || (u.protocol === 'https:' ? 443 : 80));
        const f = await forwardPort(hostId, port);
        const remote = tx('hostPort', { name: host?.config.name ?? tx('hostFallback'), port });
        u.hostname = 'localhost';
        u.port = String(f.localPort);
        return { src: u.toString(), sandboxed: false, via: `${remote} → localhost:${f.localPort}` };
      }
      // Pages on the app's own origin could call the app's APIs directly; don't open them here
      if (u.origin === location.origin) return { error: tx('thisAddressIsYiConsole') };
      return { src: u.toString(), sandboxed: false };
    })()
      .then((r) => alive && (setRes(r), setLoading(!('error' in r))))
      .catch((e) => alive && setRes({ error: String(e?.message ?? e) }));
    return () => {
      alive = false;
    };
  }, [tab, hostId, root, isLocal]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (reload) setN((x) => x + 1);
  }, [reload]);

  const go = () => {
    const v = input.trim();
    if (file && v === file) return setN((x) => x + 1);
    const next = WEB_PREFIX + normalizeUrl(v);
    if (next === tab) setN((x) => x + 1);
    else onNavigate(next);
  };
  const src = res && 'src' in res ? res.src : '';
  const external = res && 'src' in res && !res.sandboxed ? res.src : '';
  const remoteHint = !file && url && !LOOPBACK.has(safeHost(url));

  return (
    <div className="wb">
      <div className="wb-bar">
        <button className="icon-btn" title={tx('reload')} disabled={!src} onClick={() => setN((x) => x + 1)}>
          <Icon name="refresh" size={14} />
        </button>
        <form
          className="wb-addr"
          onSubmit={(e) => {
            e.preventDefault();
            go();
          }}
        >
          {loading && <span className="spinner" />}
          <input
            ref={inputRef}
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onFocus={(e) => e.target.select()}
            placeholder={isLocal ? tx('addressOrPortEG') : tx('addressOrPortRemote', { name: host?.config.name ?? tx('theHostFallback') })}
            aria-label={tx('address')}
            spellCheck={false}
          />
        </form>
        {res && 'via' in res && res.via && (
          <span className="wb-via truncate" title={tx('forwarded', { via: res.via })}>
            <Icon name="forward" size={11} /> {tx('forwarded2')}
          </span>
        )}
        <button className="icon-btn" title={tx('copyAddress')} disabled={!external} onClick={() => writeClipboard(external).then(() => toast(tx('copied')))}>
          <Icon name="copy" size={14} />
        </button>
        <button className="icon-btn" title={tx('openInSystemBrowser')} disabled={!external} onClick={() => openUrlExternal(external)}>
          <Icon name="external" size={14} />
        </button>
      </div>
      {remoteHint && src && (
        <div className="wb-tip">
          {tx('blankPageSomeSitesRefuse')}
          <button className="link-btn" onClick={() => openUrlExternal(src)}>
            {tx('openInSystemBrowser')}
          </button>
        </div>
      )}
      <div className="wb-body">
        {!url ? (
          <StartPage hostId={hostId} isLocal={isLocal} onOpen={(u) => onNavigate(WEB_PREFIX + u)} />
        ) : res && 'error' in res ? (
          <div className="wp-empty">
            <div className="wp-empty-title">{tx('cannotOpenThisAddress')}</div>
            <div className="muted small">{res.error}</div>
          </div>
        ) : src ? (
          <iframe
            key={`${src}#${n}`}
            className="wb-frame"
            src={src}
            title={webTabTitle(tab)}
            // Remote pages have their own origin and may use cookies and localStorage; HTML files on the host get no same-origin rights
            sandbox={
              res && 'sandboxed' in res && res.sandboxed
                ? 'allow-scripts allow-forms allow-modals allow-popups allow-downloads'
                : 'allow-scripts allow-same-origin allow-forms allow-modals allow-popups allow-popups-to-escape-sandbox allow-downloads'
            }
            allow="clipboard-read; clipboard-write; fullscreen"
            onLoad={() => setLoading(false)}
          />
        ) : null}
      </div>
    </div>
  );
}

function safeHost(u: string) {
  try {
    return new URL(u).hostname;
  } catch {
    return '';
  }
}

/** New tab: list the ports listening on the host; click one to open it */
function StartPage({ hostId, isLocal, onOpen }: { hostId: string; isLocal: boolean; onOpen: (url: string) => void }) {
  const [ports, setPorts] = useState<{ port: number; process?: string | null }[] | null>(null);
  useEffect(() => {
    rpc<{ ports: { port: number; process?: string | null }[] }>(hostId, 'ports.list')
      .then((r) => setPorts(r.ports))
      .catch(() => setPorts([]));
  }, [hostId]);
  return (
    <div className="wb-start">
      <div className="wb-start-title">{tx('openAPage')}</div>
      <div className="muted small">
        {tx('enterAnAddressOrPort')}
        {isLocal ? '' : tx('localhostMeansThisHostAnd')}
        {tx('rightClickAnHtmlFile')}
      </div>
      {ports && ports.length > 0 && (
        <>
          <div className="section-title">{tx('portsListeningOnTheHost')}</div>
          <div className="wb-ports">
            {ports.map((p) => (
              <button key={p.port} className="wb-port" onClick={() => onOpen(`http://localhost:${p.port}/`)}>
                <span className="mono">{p.port}</span>
                <span className="muted truncate">{p.process || ''}</span>
              </button>
            ))}
          </div>
        </>
      )}
    </div>
  );
}

