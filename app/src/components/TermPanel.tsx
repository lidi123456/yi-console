import { useEffect, useRef, useState } from 'react';
import { tr } from '../i18n';
import { errorToast, loadSession, useStore } from '../store';
import { rpc } from '../transport';
import type { SessionInfo } from '../types';
import { lsGet, lsSet } from '../util';
import { Icon } from './Icon';
import { TermTools, TerminalView } from './TerminalView';

// Terminal panel at the bottom of the session page: opens shells in the chat's working directory; several can be open, switched with tabs.
// Each terminal is a shell session whose parentId points at the chat session; not listed separately in the sidebar, deleted along with the chat

const EMPTY: SessionInfo[] = [];

export function TermPanel({ hostId, info, onClose }: { hostId: string; info: SessionInfo; onClose: () => void }) {
  const all = useStore((s) => s.sessions[hostId]) ?? EMPTY;
  const tabs = all.filter((s) => s.parentId === info.id).sort((a, b) => a.createdAt - b.createdAt);
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
        title: `${tr('终端', 'Terminal')} ${n}`,
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
    <div className="term-panel" ref={box} style={{ height: h }} aria-label={tr('终端面板', 'Terminal Panel')}>
      <div className="term-panel-resizer" onPointerDown={drag} />
      <div className="term-panel-head">
        <div className="term-tabs" role="tablist" aria-label={tr('终端列表', 'Terminal Tabs')}>
          {tabs.map((t) => {
            const exited = t.status === 'exited' || t.status === 'error';
            return (
              <div
                key={t.id}
                role="tab"
                aria-selected={t.id === cur?.id}
                className={`term-tab ${t.id === cur?.id ? 'active' : ''} ${exited ? 'exited' : ''}`}
                onClick={() => pick(t.id)}
                title={exited ? tr(`${t.title}（已退出）`, `${t.title} (Exited)`) : t.title}
              >
                <Icon name="terminal" size={13} />
                <span className="truncate">{t.title || tr('终端', 'Terminal')}</span>
                {exited && <span className="muted">{tr('已退出', 'Exited')}</span>}
                <button
                  className="term-tab-x"
                  title={tr(`删除终端 ${t.title}`, `Close terminal ${t.title}`)}
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
          <button className="icon-btn" title={tr('新建终端', 'New Terminal')} disabled={creating} onClick={create}>
            <Icon name="plus" size={15} />
          </button>
        </div>
        <TermTools />
        <button className="icon-btn" title={tr('关闭终端面板（终端在后台继续运行）', 'Hide terminal panel (processes keep running)')} onClick={onClose}>
          <Icon name="down" size={16} />
        </button>
      </div>
      <div className="term-panel-body">
        {cur ? (
          <TerminalView
            key={cur.id}
            hostId={hostId}
            sessionId={cur.id}
            running={cur.status !== 'exited' && cur.status !== 'error'}
          />
        ) : error ? (
          <div className="term-panel-empty">
            <b>{tr('无法创建终端', 'Failed to create terminal')}</b>
            <span className="muted small">{error}</span>
            <button className="btn small" onClick={create}>
              {tr('重试', 'Retry')}
            </button>
          </div>
        ) : creating ? (
          <div className="term-panel-empty muted small">{tr('正在打开终端…', 'Opening terminal…')}</div>
        ) : (
          <div className="term-panel-empty">
            <b>{tr('还没有终端', 'No terminals')}</b>
            <span className="muted small">{tr('在当前会话的工作目录中打开一个终端。', 'Open a terminal in the session working directory.')}</span>
            <button className="btn small" onClick={create}>
              <Icon name="plus" size={13} /> {tr('新建终端', 'New Terminal')}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
