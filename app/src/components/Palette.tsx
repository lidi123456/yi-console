import { useEffect, useMemo, useRef, useState } from 'react';
import { tr, useLang } from '../i18n';
import { getState, goBack, goForward, navigate, orderedSessions, setState, toggleSidebar, useStore } from '../store';
import { renameSession } from './SessionActions';
import { TOOL_LABEL } from '../types';
import { basename, fuzzyScore, timeAgo } from '../util';
import { Icon, ToolBadge } from './Icon';
import { sessionStatusClass } from './Sidebar';

interface Entry {
  key: string;
  label: string;
  sub?: string;
  icon?: string;
  tool?: string;
  cls?: string;
  run: () => void;
}

const isMac = /Mac|iPhone|iPad/.test(navigator.platform);
export const MOD = isMac ? '⌘' : 'Ctrl';

export function getShortcuts(): [string, string][] {
  return [
    [`${MOD}+K`, tr('命令面板：跳到会话、执行操作', 'Command Palette: jump to session or run action')],
    [`${MOD}+Shift+F`, tr('在所有会话的消息里搜索', 'Search messages across all sessions')],
    [`${MOD}+N`, tr('新建会话（浏览器里用 Alt+N）', 'New Session (Alt+N in browser)')],
    ['Alt+↑ / Alt+↓', tr('上一个 / 下一个会话', 'Previous / Next session')],
    ['Alt+← / Alt+→', tr('后退 / 前进', 'Back / Forward')],
    [`${MOD}+B`, tr('收起 / 展开侧边栏', 'Toggle Sidebar')],
    ['F2', tr('重命名当前会话', 'Rename current session')],
    ['Esc', tr('中断当前回合（输入框里）', 'Interrupt current turn (in input)')],
    ['Shift+Tab', tr('切换审批模式（输入框里）', 'Cycle permission mode (in input)')],
    ['/', tr('斜杠命令、提示词片段（输入框开头）', 'Slash commands & snippets (at start of input)')],
    ['@', tr('引用文件', 'Reference files')],
  ];
}

function newSession() {
  const s = getState();
  const v = s.view;
  const hostId =
    ('hostId' in v && s.hosts.some((h) => h.config.id === v.hostId && h.status.status === 'connected') ? v.hostId : null) ??
    s.hosts.find((h) => h.status.status === 'connected')?.config.id;
  if (!hostId) return;
  const cur = v.kind === 'session' ? s.sessions[v.hostId]?.find((x) => x.id === v.sessionId) : undefined;
  setState({ newSession: { hostId, cwd: cur?.cwd }, palette: false });
}

function stepSession(d: 1 | -1) {
  const list = orderedSessions();
  if (!list.length) return;
  const v = getState().view;
  const i = v.kind === 'session' ? list.findIndex((x) => x.hostId === v.hostId && x.s.id === v.sessionId) : -1;
  const n = list[(i + d + list.length) % list.length];
  navigate({ kind: 'session', hostId: n.hostId, sessionId: n.s.id });
}

/** Global shortcuts, mounted once by App */
export function useShortcuts() {
  useEffect(() => {
    const f = (e: KeyboardEvent) => {
      const mod = isMac ? e.metaKey : e.ctrlKey;
      const k = e.key.toLowerCase();
      if (mod && !e.shiftKey && k === 'k') {
        e.preventDefault();
        setState((s) => ({ palette: !s.palette }));
      } else if (mod && e.shiftKey && k === 'f') {
        e.preventDefault();
        navigate({ kind: 'search' });
      } else if ((mod && !e.shiftKey && k === 'n') || (e.altKey && !mod && k === 'n')) {
        e.preventDefault();
        newSession();
      } else if (e.altKey && !mod && (e.key === 'ArrowUp' || e.key === 'ArrowDown')) {
        e.preventDefault();
        stepSession(e.key === 'ArrowUp' ? -1 : 1);
      } else if (e.altKey && !mod && (e.key === 'ArrowLeft' || e.key === 'ArrowRight')) {
        e.preventDefault();
        if (e.key === 'ArrowLeft') goBack();
        else goForward();
      } else if (mod && !e.shiftKey && k === 'b') {
        e.preventDefault();
        toggleSidebar();
      } else if (e.key === 'F2' && !mod && !e.altKey) {
        const v = getState().view;
        const info = v.kind === 'session' ? getState().sessions[v.hostId]?.find((x) => x.id === v.sessionId) : undefined;
        if (info && v.kind === 'session') {
          e.preventDefault();
          renameSession(v.hostId, info);
        }
      }
    };
    // Mouse side buttons for back / forward
    const m = (e: MouseEvent) => {
      if (e.button === 3 || e.button === 4) {
        e.preventDefault();
        if (e.button === 3) goBack();
        else goForward();
      }
    };
    window.addEventListener('keydown', f);
    window.addEventListener('mouseup', m);
    return () => {
      window.removeEventListener('keydown', f);
      window.removeEventListener('mouseup', m);
    };
  }, []);
}

export function Palette() {
  const open = useStore((s) => s.palette);
  const hosts = useStore((s) => s.hosts);
  const sessions = useStore((s) => s.sessions);
  const [, lang] = useLang();
  const [q, setQ] = useState('');
  const [sel, setSel] = useState(0);
  const list = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (open) {
      setQ('');
      setSel(0);
    }
  }, [open]);

  const entries = useMemo(() => {
    if (!open) return [];
    const hostName = (id: string) => hosts.find((h) => h.config.id === id)?.config.name ?? '';
    const sess: Entry[] = orderedSessions().map(({ hostId, s }) => ({
      key: `s-${hostId}-${s.id}`,
      label: s.title || tr('新会话', 'New Session'),
      sub: `${TOOL_LABEL[s.tool]} · ${hostName(hostId)} · ${basename(s.cwd)} · ${timeAgo(s.updatedAt)}`,
      tool: s.tool,
      cls: sessionStatusClass(s),
      run: () => navigate({ kind: 'session', hostId, sessionId: s.id }),
    }));
    const acts: Entry[] = [
      { key: 'a-new', label: tr('新建会话', 'New Session'), icon: 'plus', sub: `${MOD}+N`, run: newSession },
      { key: 'a-home', label: tr('总览', 'Overview'), icon: 'home', run: () => navigate({ kind: 'home' }) },
      { key: 'a-hosts', label: tr('主机管理', 'Hosts'), icon: 'server', run: () => navigate({ kind: 'hosts' }) },
      { key: 'a-prefs', label: tr('系统设置', 'Preferences'), icon: 'settings', sub: tr('外观、语言、通知、网络代理', 'Appearance, language, notifications, proxy'), run: () => navigate({ kind: 'prefs' }) },
    ];
    for (const h of hosts.filter((x) => x.status.status === 'connected')) {
      const id = h.config.id;
      const n = h.config.name;
      acts.push(
        { key: `h-hist-${id}`, label: tr(`${n}：历史会话`, `${n}: Session History`), icon: 'history', run: () => navigate({ kind: 'history', hostId: id }) },
        { key: `h-files-${id}`, label: tr(`${n}：文件`, `${n}: Files`), icon: 'folder', run: () => navigate({ kind: 'files', hostId: id }) },
        { key: `h-stats-${id}`, label: tr(`${n}：用量`, `${n}: Usage Stats`), icon: 'chart', run: () => navigate({ kind: 'stats', hostId: id }) },
        { key: `h-ports-${id}`, label: tr(`${n}：端口转发`, `${n}: Port Forwarding`), icon: 'external', run: () => navigate({ kind: 'ports', hostId: id }) },
        { key: `h-set-${id}`, label: tr(`${n}：主机设置`, `${n}: Host Settings`), icon: 'settings', sub: tr('提示词片段、状态栏、推送', 'Snippets, status line, tool paths'), run: () => navigate({ kind: 'settings', hostId: id }) },
      );
    }
    const query = q.trim();
    const search: Entry = {
      key: 'a-search',
      label: query ? tr(`在消息里搜索“${query}”`, `Search messages for "${query}"`) : tr('在所有会话的消息里搜索', 'Search messages across all sessions'),
      icon: 'search',
      sub: `${MOD}+Shift+F`,
      run: () => navigate({ kind: 'search', q: query || undefined }),
    };
    if (!query) return [...sess.slice(0, 12), ...acts, search];
    const ranked = [...sess, ...acts]
      .map((e) => ({ e, s: Math.max(fuzzyScore(e.label, query) * 2, fuzzyScore(e.sub ?? '', query)) }))
      .filter((x) => x.s >= 0)
      .sort((a, b) => b.s - a.s)
      .map((x) => x.e)
      .slice(0, 40);
    return [...ranked, search];
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, q, hosts, sessions, lang]);

  useEffect(() => setSel(0), [q]);
  useEffect(() => {
    list.current?.querySelector('.active')?.scrollIntoView({ block: 'nearest' });
  }, [sel]);

  if (!open) return null;
  const close = () => setState({ palette: false });
  const pick = (e?: Entry) => {
    if (!e) return;
    close();
    e.run();
  };
  return (
    <div className="palette-scrim" onMouseDown={close}>
      <div className="palette" onMouseDown={(e) => e.stopPropagation()}>
        <div className="palette-input">
          <Icon name="search" size={16} />
          <input
            autoFocus
            value={q}
            placeholder={tr('跳到会话，或输入操作…', 'Jump to session, or type a command…')}
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => {
              if (e.nativeEvent.isComposing) return;
              if (e.key === 'Escape') close();
              else if (e.key === 'ArrowDown') {
                e.preventDefault();
                setSel((sel + 1) % entries.length);
              } else if (e.key === 'ArrowUp') {
                e.preventDefault();
                setSel((sel - 1 + entries.length) % entries.length);
              } else if (e.key === 'Enter') {
                e.preventDefault();
                pick(entries[sel]);
              }
            }}
          />
        </div>
        <div className="palette-list" ref={list}>
          {entries.map((e, i) => (
            <button
              key={e.key}
              className={`palette-item ${i === sel ? 'active' : ''}`}
              onMouseEnter={() => setSel(i)}
              onClick={() => pick(e)}
            >
              {e.tool ? <ToolBadge tool={e.tool} size={18} /> : <Icon name={e.icon ?? 'chat'} size={16} />}
              <span className="palette-label truncate">{e.label}</span>
              {e.cls && <span className={`sdot sdot-${e.cls}`} />}
              {e.sub && <span className="palette-sub truncate">{e.sub}</span>}
            </button>
          ))}
        </div>
        <div className="palette-foot hide-mobile">
          {getShortcuts().slice(0, 5).map(([k, d]) => (
            <span key={k}>
              <kbd>{k}</kbd> {d.split(/（|\(/)[0].trim()}
            </span>
          ))}
        </div>
      </div>
    </div>
  );
}
