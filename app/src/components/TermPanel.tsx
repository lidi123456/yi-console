import { useEffect, useRef, useState } from 'react';
import { getT } from '../i18n';
import { errorToast, getSession, loadSession, skey, useStore } from '../store';
import { rpc } from '../transport';
import type { SessionInfo } from '../types';
import { lsGet, lsSet } from '../util';
import { Icon } from './Icon';
import { TermTools, TerminalView } from './TerminalView';

const tx = getT('termPanel');

// Terminal panel at the bottom of the session page: opens shells in the chat's working directory; several can be open, switched with tabs.
// Each terminal is a shell session whose parentId points at the chat session; not listed separately in the sidebar, deleted along with the chat.
// Like VS Code's integrated terminal these are plain ptys, not tmux (yi-agentd skips tmux when parentId is set), so they
// end when yi-agentd restarts. Exits follow VS Code: a clean exit (`exit`, Ctrl+D, or a daemon restart) closes the tab,
// an abnormal one keeps it with the reason and a restart button; opening the panel with no live terminal starts one.

const EMPTY: SessionInfo[] = [];

export function TermPanel({ hostId, info, onClose }: { hostId: string; info: SessionInfo; onClose: () => void }) {
  const all = useStore((s) => s.sessions[hostId]) ?? EMPTY;
  const mine = all.filter((s) => s.parentId === info.id).sort((a, b) => a.createdAt - b.createdAt);
  // Cleanly exited terminals are being closed (below): leave them out right away
  const tabs = mine.filter((s) => s.status !== 'exited');
  const [active, setActive] = useState<string>(() => lsGet(`term-tab-${info.id}`, ''));
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState('');
  const cur = tabs.find((t) => t.id === active) ?? tabs[tabs.length - 1];

  const pick = (id: string) => {
    lsSet(`term-tab-${info.id}`, id);
    setActive(id);
  };

  const create = async () => {
    if (creating) return;
    setCreating(true);
    setError('');
    try {
      const n = tabs.length + 1;
      const s = await rpc<SessionInfo>(hostId, 'session.create', {
        tool: 'shell',
        cwd: info.cwd,
        mode: 'terminal',
        title: `${tx('terminal')} ${n}`,
        parentId: info.id,
        cols: 120,
        rows: 12,
      });
      pick(s.id);
    } catch (e) {
      setError(String((e as Error)?.message ?? e));
    } finally {
      setCreating(false);
    }
  };

  const remove = (id: string) => rpc(hostId, 'session.delete', { sessionId: id }).catch(errorToast);

  // Close cleanly exited terminals, like VS Code closing the tab when the shell exits with code 0
  const closing = useRef(new Set<string>());
  useEffect(() => {
    for (const s of mine) {
      if (s.status !== 'exited' || closing.current.has(s.id)) continue;
      closing.current.add(s.id);
      rpc(hostId, 'session.delete', { sessionId: s.id }).catch(() => closing.current.delete(s.id));
    }
  });

  const restart = (id: string) => rpc(hostId, 'session.resume', { sessionId: id, cols: 120, rows: 12 }).catch(errorToast);

  // Open one automatically the first time if there are no terminals
  const auto = useRef(false);
  useEffect(() => {
    if (auto.current || tabs.length) return;
    auto.current = true;
    create();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (cur) loadSession(hostId, cur.id).catch(() => {});
  }, [hostId, cur?.id]);

  // ---------- Drag the top edge to resize ----------
  const [h, setH] = useState(() => lsGet('term-panel-h', 280));
  const box = useRef<HTMLDivElement>(null);
  const drag = (e: React.PointerEvent) => {
    e.preventDefault();
    const y0 = e.clientY;
    const h0 = h;
    const max = (box.current?.parentElement?.clientHeight ?? 800) - 200;
    let v = h0;
    const move = (ev: PointerEvent) => {
      v = Math.max(120, Math.min(max, h0 - (ev.clientY - y0)));
      setH(v);
    };
    const up = () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
      document.body.classList.remove('resizing-y');
      lsSet('term-panel-h', v);
    };
    document.body.classList.add('resizing-y');
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  };

  return (
    <div className="term-panel" ref={box} style={{ height: h }} aria-label={tx('terminalPanel')}>
      <div className="term-panel-resizer" onPointerDown={drag} />
      <div className="term-panel-head">
        <div className="term-tabs" role="tablist" aria-label={tx('terminalTabs')}>
          {tabs.map((t) => {
            const exited = t.status === 'error';
            return (
              <div
                key={t.id}
                role="tab"
                aria-selected={t.id === cur?.id}
                className={`term-tab ${t.id === cur?.id ? 'active' : ''} ${exited ? 'exited' : ''}`}
                onClick={() => pick(t.id)}
                title={exited ? tx('exited', { title: t.title }) : t.title}
              >
                <Icon name="terminal" size={13} />
                <span className="truncate">{t.title || tx('terminal')}</span>
                {exited && <span className="muted">{tx('exited2')}</span>}
                <button
                  className="term-tab-x"
                  title={tx('closeTerminal', { title: t.title })}
                  onClick={(e) => {
                    e.stopPropagation();
                    remove(t.id);
                  }}
                >
                  <Icon name="x" size={12} />
                </button>
              </div>
            );
          })}
          <button className="icon-btn" title={tx('newTerminal')} disabled={creating} onClick={create}>
            <Icon name="plus" size={15} />
          </button>
        </div>
        <TermTools />
        <button className="icon-btn" title={tx('hideTerminalPanelProcessesKeep')} onClick={onClose}>
          <Icon name="down" size={16} />
        </button>
      </div>
      <div className="term-panel-body">
        {cur ? (
          <>
            <TerminalView key={cur.id} hostId={hostId} sessionId={cur.id} running={cur.status !== 'error'} />
            {cur.status === 'error' && (
              <div className="term-panel-exit">
                <span>{tx('processExited', { reason: lastError(hostId, cur.id) || tx('unknownReason') })}</span>
                <button className="btn small" onClick={() => restart(cur.id)}>
                  {tx('restart')}
                </button>
                <button className="btn small" onClick={() => remove(cur.id)}>
                  {tx('close')}
                </button>
              </div>
            )}
          </>
        ) : error ? (
          <div className="term-panel-empty">
            <b>{tx('failedToCreateTerminal')}</b>
            <span className="muted small">{error}</span>
            <button className="btn small" onClick={create}>
              {tx('retry')}
            </button>
          </div>
        ) : creating ? (
          <div className="term-panel-empty muted small">{tx('openingTerminal')}</div>
        ) : (
          <div className="term-panel-empty">
            <b>{tx('noTerminals')}</b>
            <span className="muted small">{tx('openATerminalInThe')}</span>
            <button className="btn small" onClick={create}>
              <Icon name="plus" size={13} /> {tx('newTerminal')}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

/** The error yi-agentd recorded when the process exited (e.g. "进程退出，退出码 1") */
function lastError(hostId: string, sid: string): string {
  const ev = getSession(skey(hostId, sid)).events;
  for (let i = ev.length - 1; i >= 0; i--) {
    const e = ev[i];
    if (e.kind === 'error') return e.message;
  }
  return '';
}
