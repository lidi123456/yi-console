import { useEffect, useMemo, useRef, useState } from 'react';
import { getT, useLang } from '../i18n';
import { getState, goBack, goForward, navigate, orderedSessions, setState, toggleSidebar, useStore } from '../store';
import { renameSession } from './SessionActions';
import { TOOL_LABEL } from '../types';
import { basename, fuzzyScore, timeAgo } from '../util';
import { Icon, ToolBadge } from './Icon';
import { sessionStatusClass } from './Sidebar';

const t = getT('palette');

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
    [`${MOD}+K`, t('shortcuts.palette')],
    [`${MOD}+Shift+F`, t('search.all')],
    [`${MOD}+N`, t('shortcuts.newSession')],
    ['Alt+↑ / Alt+↓', t('shortcuts.switchSession')],
    ['Alt+← / Alt+→', t('shortcuts.backForward')],
    [`${MOD}+B`, t('shortcuts.sidebar')],
    ['F2', t('shortcuts.rename')],
    ['Esc', t('shortcuts.interrupt')],
    ['Shift+Tab', t('shortcuts.permissionMode')],
    ['/', t('shortcuts.slash')],
    ['@', t('shortcuts.mention')],
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
      label: s.title || t('untitledSession'),
      sub: `${TOOL_LABEL[s.tool]} · ${hostName(hostId)} · ${basename(s.cwd)} · ${timeAgo(s.updatedAt)}`,
      tool: s.tool,
      cls: sessionStatusClass(s),
      run: () => navigate({ kind: 'session', hostId, sessionId: s.id }),
    }));
    const acts: Entry[] = [
      { key: 'a-new', label: t('actions.newSession'), icon: 'plus', sub: `${MOD}+N`, run: newSession },
      { key: 'a-home', label: t('actions.overview'), icon: 'home', run: () => navigate({ kind: 'home' }) },
      { key: 'a-hosts', label: t('actions.hosts'), icon: 'server', run: () => navigate({ kind: 'hosts' }) },
      { key: 'a-prefs', label: t('actions.prefs'), icon: 'settings', sub: t('actions.prefsSub'), run: () => navigate({ kind: 'prefs' }) },
    ];
    for (const h of hosts.filter((x) => x.status.status === 'connected')) {
      const id = h.config.id;
      const n = h.config.name;
      acts.push(
        { key: `h-hist-${id}`, label: t('host.history', { name: n }), icon: 'history', run: () => navigate({ kind: 'history', hostId: id }) },
        { key: `h-files-${id}`, label: t('host.files', { name: n }), icon: 'folder', run: () => navigate({ kind: 'files', hostId: id }) },
        { key: `h-stats-${id}`, label: t('host.stats', { name: n }), icon: 'chart', run: () => navigate({ kind: 'stats', hostId: id }) },
        { key: `h-ports-${id}`, label: t('host.ports', { name: n }), icon: 'external', run: () => navigate({ kind: 'ports', hostId: id }) },
        { key: `h-set-${id}`, label: t('host.settings', { name: n }), icon: 'settings', sub: t('host.settingsSub'), run: () => navigate({ kind: 'settings', hostId: id }) },
      );
    }
    const query = q.trim();
    const search: Entry = {
      key: 'a-search',
      label: query ? t('search.query', { query }) : t('search.all'),
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
            placeholder={t('placeholder')}
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
