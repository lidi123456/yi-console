import { useEffect, useState } from 'react';
import { tr } from '../i18n';
import { errorToast, toast, useStore } from '../store';
import { hubCall, isTauri, onHubEvent, rpc } from '../transport';
import { writeClipboard } from '../clipboard';
import { Icon } from './Icon';
import { errText } from './StatsView';

// Port forwarding: connect a port on the host to 127.0.0.1 on this machine so dev servers on remote machines open in the local browser.
// SSH hosts use SSH's own port forwarding; relay hosts go through a relay channel, with yi-agentd connecting to its local port (yi-agentd itself listens on no ports).

export interface ForwardInfo {
  hostId: string;
  port: number;
  localPort: number;
  direct: boolean;
  conns: number;
  error: string | null;
  label: string | null;
}

interface Listening {
  port: number;
  addr: string;
  pid?: number | null;
  process?: string | null;
}

export const isMobileApp = isTauri && /Android|iPhone|iPad/i.test(navigator.userAgent);

/** Open a URL: the system browser on desktop, the in-app browser in the mobile app (Android Custom Tabs, whose menu can switch to another browser), a new tab in the browser build */
export function openUrl(url: string) {
  // On mobile ClientHub can't open the system browser, so hand it to the opener plugin
  if (isMobileApp) import('@tauri-apps/plugin-opener').then((m) => m.openUrl(url, 'inAppBrowser')).catch(errorToast);
  else if (isTauri) hubCall('url.open', { url }).catch(errorToast);
  else window.open(url, '_blank', 'noopener');
}

/** Open in another browser on mobile: Android shows the system app chooser (YiConsoleAndroid injected by MainActivity), otherwise the default browser */
export function openUrlExternal(url: string) {
  const native = (window as { YiConsoleAndroid?: { openExternal(u: string): void } }).YiConsoleAndroid;
  if (isMobileApp && native) native.openExternal(url);
  else if (isMobileApp) import('@tauri-apps/plugin-opener').then((m) => m.openUrl(url)).catch(errorToast);
  else openUrl(url);
}

export const forwardUrl = (f: ForwardInfo) => `http://localhost:${f.localPort}/`;

/** Forward a port on the host (the local host needs no forwarding and gets the original port back) */
export async function forwardPort(hostId: string, port: number, localPort?: number): Promise<ForwardInfo> {
  return hubCall<ForwardInfo>('ports.forward', { id: hostId, port, ...(localPort ? { localPort } : {}) });
}

export function PortsView({ hostId }: { hostId: string }) {
  const host = useStore((s) => s.hosts.find((h) => h.config.id === hostId));
  const connected = host?.status.status === 'connected';
  const isLocal = host?.config.kind === 'local';
  const [forwards, setForwards] = useState<ForwardInfo[]>([]);
  const [listening, setListening] = useState<Listening[] | null>(null);
  const [listErr, setListErr] = useState('');
  const [port, setPort] = useState('');
  const [localPort, setLocalPort] = useState('');
  const [busy, setBusy] = useState(false);

  const loadForwards = () =>
    hubCall<ForwardInfo[]>('ports.forwards', { id: hostId })
      .then(setForwards)
      .catch(() => {});
  const loadListening = () =>
    rpc<{ ports: Listening[] }>(hostId, 'ports.list')
      .then((r) => {
        setListening(r.ports);
        setListErr('');
      })
      .catch((e) => setListErr(errText(e)));

  useEffect(() => {
    loadForwards();
    const off = onHubEvent((ev) => ev.type === 'ports.changed' && loadForwards());
    // Connection counts and errors change in the client and aren't in events; refresh periodically while the page is open
    const t = setInterval(loadForwards, 3000);
    return () => {
      off();
      clearInterval(t);
    };
  }, [hostId]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (!connected) return;
    loadListening();
    const t = setInterval(loadListening, 5000);
    return () => clearInterval(t);
  }, [hostId, connected]); // eslint-disable-line react-hooks/exhaustive-deps

  const add = async (p: number, lp?: number, open = false) => {
    setBusy(true);
    try {
      const f = await forwardPort(hostId, p, lp);
      setPort('');
      setLocalPort('');
      await loadForwards();
      if (open) openUrl(forwardUrl(f));
      else if (!f.direct) toast(tr(`已转发：localhost:${f.localPort} → ${host?.config.name ?? ''} 的 ${p} 端口`, `Forwarded localhost:${f.localPort} → ${host?.config.name ?? ''} port ${p}`));
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };
  const stop = async (f: ForwardInfo) => {
    await hubCall('ports.unforward', { id: hostId, port: f.port }).catch(errorToast);
    loadForwards();
  };
  const submit = () => {
    const p = Number(port);
    const lp = localPort ? Number(localPort) : undefined;
    if (!Number.isInteger(p) || p < 1 || p > 65535) return errorToast(tr('端口号要在 1–65535 之间', 'Port must be between 1 and 65535'));
    if (lp !== undefined && (!Number.isInteger(lp) || lp < 1 || lp > 65535)) return errorToast(tr('本机端口号要在 1–65535 之间', 'Local port must be between 1 and 65535'));
    add(p, lp);
  };

  const byPort = new Map(forwards.map((f) => [f.port, f]));
  const loopback = (a: string) => a.startsWith('127.') || a === '::1';

  return (
    <div className="page ports">
      <div className="page-head">
        <h2>{tr('端口转发', 'Port forwarding')} · {host?.config.name ?? hostId}</h2>
        <button className="btn small" onClick={() => (loadForwards(), connected && loadListening())}>
          <Icon name="refresh" size={14} /> {tr('刷新', 'Refresh')}
        </button>
      </div>
      <p className="muted small">
        {isLocal
          ? tr('这是本机，端口不用转发，可以直接在浏览器里打开。', 'This is the local machine. Open ports directly in the browser.')
          : tr(
              `把 ${host?.config.name ?? '主机'} 上的端口接到本机的 localhost，在本机浏览器里打开远程机器上跑的网页、接口。只转发到主机自己的 127.0.0.1，本机这边也只监听 127.0.0.1，别的电脑连不进来。转发在关闭 App 后结束。`,
              `Connect a port on ${host?.config.name ?? 'the host'} to localhost so you can open remote pages and APIs in this browser. Traffic only reaches 127.0.0.1 on the host, and this side only listens on 127.0.0.1. Forwards end when the app closes.`,
            )}
        {!isTauri && tr(' 浏览器版里，本机端口开在运行 yi-web 的那台机器上。', ' In the browser build, the local port is opened on the machine running yi-web.')}
      </p>

      {!isLocal && (
        <div className="card ports-add">
          <div className="row">
            <label className="ports-field">
              <span>{tr('主机端口', 'Host port')}</span>
              <input value={port} onChange={(e) => setPort(e.target.value.replace(/\D/g, ''))} placeholder={tr('如 3000', 'e.g. 3000')} inputMode="numeric" onKeyDown={(e) => e.key === 'Enter' && submit()} />
            </label>
            <label className="ports-field">
              <span>{tr('本机端口（可不填）', 'Local port (optional)')}</span>
              <input value={localPort} onChange={(e) => setLocalPort(e.target.value.replace(/\D/g, ''))} placeholder={tr('默认同号，被占用时自动换', 'Same number by default; picks another if busy')} inputMode="numeric" onKeyDown={(e) => e.key === 'Enter' && submit()} />
            </label>
            <button className="btn primary" disabled={busy || !port || !connected} onClick={submit}>
              {tr('转发', 'Forward')}
            </button>
          </div>
          {!connected && <div className="muted small">{tr('主机未连接，连上后才能转发。', 'Host is offline. Connect it before forwarding.')}</div>}
        </div>
      )}

      {!isLocal && (
        <>
          <div className="section-title">{tr('已转发', 'Forwarded')}</div>
          {forwards.length === 0 ? (
            <div className="muted small ports-none">{tr('还没有转发的端口。在上面填端口号，或者在下面的列表里点「转发」。', 'Nothing forwarded yet. Enter a port above, or click Forward in the list.')}</div>
          ) : (
            <div className="ports-list">
              {forwards.map((f) => (
                <div key={f.port} className="ports-row">
                  <span className="ports-port mono">{f.port}</span>
                  <Icon name="forward" size={12} />
                  <button className="link-btn mono" title={tr('在浏览器里打开', 'Open in browser')} onClick={() => openUrl(forwardUrl(f))}>
                    localhost:{f.localPort}
                  </button>
                  <span className="grow ports-note truncate">
                    {f.error ? <span className="ports-err" title={f.error}>{f.error}</span> : f.conns > 0 ? <span className="muted">{tr(`${f.conns} 个连接`, `${f.conns} connections`)}</span> : null}
                  </span>
                  <button className="icon-btn" title={tr('在浏览器里打开', 'Open in browser')} onClick={() => openUrl(forwardUrl(f))}>
                    <Icon name="external" size={14} />
                  </button>
                  <button className="icon-btn" title={tr('复制地址', 'Copy address')} onClick={() => writeClipboard(forwardUrl(f)).then(() => toast(tr('已复制', 'Copied')))}>
                    <Icon name="copy" size={14} />
                  </button>
                  <button className="icon-btn" title={tr('停止转发', 'Stop forwarding')} onClick={() => stop(f)}>
                    <Icon name="x" size={14} />
                  </button>
                </div>
              ))}
            </div>
          )}
        </>
      )}

      <div className="section-title">{tr('主机上正在监听的端口', 'Ports listening on the host')}</div>
      {!connected ? (
        <div className="muted small ports-none">{tr('主机未连接。', 'Host is offline.')}</div>
      ) : listErr ? (
        <div className="muted small ports-none">{listErr}</div>
      ) : listening === null ? (
        <div className="muted small ports-none">{tr('正在读取…', 'Loading…')}</div>
      ) : listening.length === 0 ? (
        <div className="muted small ports-none">{tr('没有找到监听中的端口。', 'No listening ports found.')}</div>
      ) : (
        <div className="ports-list">
          {listening.map((l) => {
            const f = byPort.get(l.port);
            return (
              <div key={l.port} className="ports-row">
                <span className="ports-port mono">{l.port}</span>
                <span className="ports-proc truncate" title={l.pid ? tr(`进程 ${l.pid}`, `pid ${l.pid}`) : undefined}>
                  {l.process || <span className="muted">{tr('未知进程', 'Unknown process')}</span>}
                </span>
                <span className="grow muted small mono truncate">{loopback(l.addr) ? tr('仅本机', 'Local only') : l.addr === '::' || l.addr === '0.0.0.0' ? tr('所有地址', 'All addresses') : l.addr}</span>
                {isLocal ? (
                  <button className="btn small" onClick={() => openUrl(`http://localhost:${l.port}/`)}>
                    {tr('打开', 'Open')}
                  </button>
                ) : f ? (
                  <button className="btn small" onClick={() => openUrl(forwardUrl(f))} title={tr(`已转发到 localhost:${f.localPort}`, `Forwarded to localhost:${f.localPort}`)}>
                    {tr('打开', 'Open')}
                  </button>
                ) : (
                  <>
                    <button className="btn small" disabled={busy} onClick={() => add(l.port)}>
                      {tr('转发', 'Forward')}
                    </button>
                    <button className="btn small" disabled={busy} onClick={() => add(l.port, undefined, true)} title={tr('转发后在浏览器里打开', 'Forward and open in the browser')}>
                      {tr('转发并打开', 'Forward and open')}
                    </button>
                  </>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
