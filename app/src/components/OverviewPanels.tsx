// The parts of the overview page around the board: quick start with drafts, projects, activity, the last 7 days and
// the quota dialog. Everything that asks a host for data does so only while the page is visible and the panel is on
// screen, and caches the answer (see docs/internals/frontend-ui.md, "Overview").
import { useEffect, useMemo, useRef, useState } from 'react';
import { useBack } from '../back';
import { getT, useLang } from '../i18n';
import { addDraft, projKey, removeDraft, useDrafts, useFeed, usePageVisible, type Draft, type FeedItem } from '../overview';
import { permLabel, permModes } from '../perm';
import { errorToast, navigate, openNewSession, toast, useStore, type HostEntry } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type GitStatus, type SessionInfo, type ToolKind, type Usage } from '../types';
import { basename, clock, fmtTokens, isMobile, lsGet, lsSet, shortPath, timeAgo } from '../util';
import type { Item } from './Home';
import { Icon, ToolBadge } from './Icon';
import { DirPicker, PREFS_KEY, recentDirs, rememberDir, type ToolPrefs } from './NewSession';
import { openMenu, type MenuItem } from './Overlay';
import { showChanges } from './SessionActions';
import { QUOTA_TOOLS, QuotaCard, TOOL_VAR } from './StatsView';

const t = getT('home');

/** Projects listed under the board */
const PROJ_MAX = 6;
/** git.status / stats.usage answers are reused this long (git also refreshes when a turn in that directory ends) */
const GIT_TTL = 10 * 60_000;
const STATS_TTL = 10 * 60_000;
const FEED_SHOWN = 6;
const DIR_MENU_MAX = 10;

const menuUnder = (e: React.MouseEvent<HTMLElement>, items: (MenuItem | '-')[]) => {
  const r = e.currentTarget.getBoundingClientRect();
  openMenu({ clientX: r.left, clientY: r.bottom + 4 }, items);
};

/** True once the element has come near the viewport (stays true) */
function useInView(ref: React.RefObject<HTMLElement | null>) {
  const [v, setV] = useState(false);
  useEffect(() => {
    if (v || !ref.current) return;
    if (typeof IntersectionObserver === 'undefined') {
      setV(true);
      return;
    }
    const o = new IntersectionObserver((es) => {
      if (es.some((e) => e.isIntersecting)) {
        setV(true);
        o.disconnect();
      }
    }, { rootMargin: '200px' });
    o.observe(ref.current);
    return () => o.disconnect();
  }, [v, ref]);
  return v;
}

const isConnected = (h: HostEntry) => h.status.status === 'connected';
const aiTools = (h?: HostEntry): ToolKind[] =>
  h?.status.hello?.tools.filter((x) => x.available && x.kind !== 'shell' && x.kind !== 'files').map((x) => x.kind) ?? [];

/** Directories for the quick start menu: used recently from the new session page, then those of the host's sessions */
function dirList(hostId: string, sessions: Record<string, SessionInfo[]>) {
  const fromSessions = (sessions[hostId] ?? [])
    .filter((s) => !s.parentId)
    .sort((a, b) => b.updatedAt - a.updatedAt)
    .map((s) => s.cwd);
  return [...new Set([...recentDirs(hostId), ...fromSessions])].slice(0, DIR_MENU_MAX);
}

/** Start a chat session with a first message; the board shows it, so the page stays where it is */
async function startSession(d: Pick<Draft, 'text' | 'hostId' | 'cwd' | 'tool' | 'perm'>): Promise<boolean> {
  const prefs = lsGet<Record<string, ToolPrefs>>(PREFS_KEY, {})[d.tool] ?? {};
  try {
    const info = await rpc<SessionInfo>(d.hostId, 'session.create', {
      tool: d.tool,
      cwd: d.cwd,
      mode: 'structured',
      prompt: d.text,
      permissionMode: permModes(d.tool, 'structured').some((m) => m[0] === d.perm) ? d.perm : 'default',
      // The model and thinking level last chosen for this tool on the new session page
      model: prefs.model || undefined,
      effort: prefs.model ? prefs.effort || undefined : undefined,
      cols: 120,
      rows: 32,
    });
    rememberDir(d.hostId, info.cwd);
    lsSet('last-tool', d.tool);
    toast(t('sessionStarted'), 'info', { label: t('view'), run: () => navigate({ kind: 'session', hostId: d.hostId, sessionId: info.id }) });
    return true;
  } catch (e) {
    errorToast(e);
    return false;
  }
}

export function QuickStart({ hosts, sessions }: { hosts: HostEntry[]; sessions: Record<string, SessionInfo[]> }) {
  const [, lang] = useLang();
  const connected = hosts.filter(isConnected);
  const [hostPick, setHostPick] = useState(() => lsGet('ov-qs-host', ''));
  const host = connected.find((h) => h.config.id === hostPick) ?? connected[0];
  const hostId = host?.config.id ?? '';
  const tools = aiTools(host);
  const [toolPick, setToolPick] = useState<ToolKind>(() => lsGet<ToolKind>('last-tool', 'claude'));
  const tool: ToolKind | undefined = tools.includes(toolPick) ? toolPick : tools[0];
  const dirs = useMemo(() => dirList(hostId, sessions), [hostId, sessions]);
  const [cwdPick, setCwdPick] = useState<Record<string, string>>({});
  const cwd = cwdPick[hostId] ?? dirs[0] ?? '~';
  const [permPick, setPermPick] = useState<Record<string, string>>({});
  const perms = useMemo(() => (tool ? permModes(tool, 'structured') : []), [tool, lang]);
  const perm = useMemo(() => {
    const saved = tool ? lsGet<Record<string, ToolPrefs>>(PREFS_KEY, {})[tool]?.perm : undefined;
    return [tool && permPick[tool], saved].find((p) => p && perms.some((m) => m[0] === p)) || 'default';
  }, [tool, permPick, perms]);
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const [picking, setPicking] = useState(false);
  const ta = useRef<HTMLTextAreaElement>(null);
  const drafts = useDrafts();
  useBack(picking, () => setPicking(false));

  const ready = !!host && !!tool && !busy;
  const pickHost = (id: string) => {
    setHostPick(id);
    lsSet('ov-qs-host', id);
  };
  const go = async () => {
    const v = text.trim();
    if (!v || !ready || !tool) return;
    setBusy(true);
    if (await startSession({ text: v, hostId, cwd, tool, perm })) setText('');
    setBusy(false);
  };
  const save = () => {
    const v = text.trim();
    if (!v || !host || !tool) {
      ta.current?.focus();
      return;
    }
    addDraft({ text: v, hostId, cwd, tool, perm });
    setText('');
    toast(t('draftSaved'));
  };
  const edit = (d: Draft) => {
    // Whatever is in the box isn't thrown away: it becomes a draft in turn
    if (text.trim() && host && tool) addDraft({ text: text.trim(), hostId, cwd, tool, perm });
    removeDraft(d.id);
    setText(d.text);
    pickHost(d.hostId);
    setToolPick(d.tool);
    setCwdPick((c) => ({ ...c, [d.hostId]: d.cwd }));
    setPermPick((p) => ({ ...p, [d.tool]: d.perm }));
    ta.current?.focus();
  };
  const del = (d: Draft) => {
    removeDraft(d.id);
    toast(t('draftDeleted'), 'info', { label: t('undo'), run: () => addDraft(d) });
  };
  const startDraft = async (d: Draft) => {
    removeDraft(d.id);
    if (!(await startSession(d))) addDraft(d);
  };

  if (connected.length === 0 && drafts.length === 0) return null;
  return (
    <>
      {connected.length > 0 && (
        <div className="ov-qs">
          <textarea
            ref={ta}
            value={text}
            rows={2}
            placeholder={tool ? t('quickStartPlaceholder') : t('noAiToolOnThisHost')}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (e.nativeEvent.isComposing) return;
              if (e.key === 'Enter' && !e.shiftKey && !isMobile()) {
                e.preventDefault();
                go();
              }
            }}
          />
          <div className="ov-qs-row">
            <button
              className="ov-pick"
              disabled={!tools.length}
              onClick={(e) => menuUnder(e, tools.map((k) => ({ label: TOOL_LABEL[k], tool: k, active: k === tool, run: () => setToolPick(k) })))}
            >
              {tool && <ToolBadge tool={tool} size={14} />}
              {tool ? TOOL_LABEL[tool] : t('noAiTool')}
              <Icon name="down" size={11} />
            </button>
            {connected.length > 1 && (
              <button
                className="ov-pick"
                onClick={(e) =>
                  menuUnder(
                    e,
                    connected.map((h) => ({ label: h.config.name, active: h.config.id === hostId, run: () => pickHost(h.config.id) })),
                  )
                }
              >
                {host?.config.name}
                <Icon name="down" size={11} />
              </button>
            )}
            <button
              className="ov-pick ov-pick-dir"
              title={cwd}
              onClick={(e) =>
                menuUnder(e, [
                  ...dirs.map((d) => ({
                    label: shortPath(d, host?.status.hello?.home),
                    active: d === cwd,
                    run: () => setCwdPick((c) => ({ ...c, [hostId]: d })),
                  })),
                  ...(dirs.length ? (['-'] as const) : []),
                  { label: t('chooseDirectory'), icon: 'folder', run: () => setPicking(true) },
                ])
              }
            >
              <span className="truncate">{shortPath(cwd, host?.status.hello?.home)}</span>
              <Icon name="down" size={11} />
            </button>
            {tool && perms.length > 0 && (
              <button
                className={`ov-pick perm-${perm}`}
                onClick={(e) =>
                  menuUnder(
                    e,
                    perms.map(([id, label]) => ({ label, active: id === perm, run: () => setPermPick((p) => ({ ...p, [tool]: id })) })),
                  )
                }
              >
                <Icon name="shield" size={12} />
                {permLabel(perm)}
                <Icon name="down" size={11} />
              </button>
            )}
            <span className="grow" />
            <button className="btn small" disabled={!text.trim() || !tool} onClick={save}>
              {t('saveDraft')}
            </button>
            <button className="btn small primary" disabled={!text.trim() || !ready} onClick={go}>
              {busy ? <span className="spinner" /> : t('start')}
            </button>
          </div>
        </div>
      )}
      {drafts.length > 0 && (
        <div className="ov-drafts">
          <div className="ov-drafts-h">
            {t('drafts')}
            <span className="ov-n">{drafts.length}</span>
            <span className="muted small">{t('draftsStayOnThisDevice')}</span>
          </div>
          <div className="ov-drafts-row">
            {drafts.map((d) => {
              const h = hosts.find((x) => x.config.id === d.hostId);
              const up = !!h && isConnected(h);
              const can = up && aiTools(h).includes(d.tool);
              return (
                <div key={d.id} className="ov-draft">
                  <div className="ov-draft-t" title={d.text}>
                    {d.text}
                  </div>
                  <div className="ov-draft-m">
                    <ToolBadge tool={d.tool} size={14} />
                    <span className="truncate">
                      {h ? `${h.config.name} · ` : ''}
                      {shortPath(d.cwd, h?.status.hello?.home)}
                    </span>
                  </div>
                  {!can && <div className="ov-draft-note">{!h ? t('hostRemoved') : !up ? t('hostNotConnected') : t('toolNotAvailable')}</div>}
                  <div className="ov-draft-a">
                    <button className="ov-link" onClick={() => del(d)}>
                      {t('delete')}
                    </button>
                    <span className="grow" />
                    <button className="btn small" onClick={() => edit(d)}>
                      {t('edit')}
                    </button>
                    <button className="btn small primary" disabled={!can} onClick={() => startDraft(d)}>
                      {t('start')}
                    </button>
                  </div>
                </div>
              );
            })}
          </div>
        </div>
      )}
      {picking && host && (
        <div className="modal-bg" onClick={() => setPicking(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <div className="modal-head">
              <h3>{t('workingDirectory')}</h3>
              <button className="icon-btn" onClick={() => setPicking(false)} aria-label={t('close')}>
                <Icon name="x" />
              </button>
            </div>
            <DirPicker
              hostId={hostId}
              start={cwd}
              onPick={(p) => {
                setCwdPick((c) => ({ ...c, [hostId]: p }));
                setPicking(false);
              }}
              onClose={() => setPicking(false)}
            />
          </div>
        </div>
      )}
    </>
  );
}

// ---------- Projects ----------

interface Project {
  key: string;
  hostId: string;
  hostName: string;
  home?: string;
  cwd: string;
  tools: ToolKind[];
  last: number;
  /** Latest turn end in the directory: git status is refetched when it changes */
  lastEnd: number;
  /** Most recently updated session there (opens "Changes") */
  latest: Item;
}

const gitCache = new Map<string, { at: number; lastEnd: number; v: GitStatus | null }>();
const gitInflight = new Set<string>();

export function ProjectPanel({
  items,
  filter,
  setFilter,
  multi,
}: {
  items: Item[];
  filter: string | null;
  setFilter: (k: string | null) => void;
  multi: boolean;
}) {
  const hosts = useStore((s) => s.hosts);
  const projects = useMemo(() => {
    const by = new Map<string, Project>();
    for (const x of items) {
      const key = projKey(x.hostId, x.s.cwd);
      let p = by.get(key);
      if (!p) {
        p = { key, hostId: x.hostId, hostName: x.hostName, home: x.home, cwd: x.s.cwd, tools: [], last: 0, lastEnd: 0, latest: x };
        by.set(key, p);
      }
      if (!p.tools.includes(x.s.tool)) p.tools.push(x.s.tool);
      if (x.s.updatedAt > p.last) {
        p.last = x.s.updatedAt;
        p.latest = x;
      }
      p.lastEnd = Math.max(p.lastEnd, x.s.turn?.endedAt ?? 0);
    }
    return [...by.values()].sort((a, b) => b.last - a.last).slice(0, PROJ_MAX);
  }, [items]);
  const ref = useRef<HTMLElement>(null);
  const inView = useInView(ref);
  const visible = usePageVisible();
  const [, setTick] = useState(0);
  const [force, setForce] = useState(0);
  const up = (hostId: string) => hosts.some((h) => h.config.id === hostId && isConnected(h));

  useEffect(() => {
    if (!inView || !visible) return;
    for (const p of projects) {
      const c = gitCache.get(p.key);
      const stale = !c || c.lastEnd !== p.lastEnd || Date.now() - c.at > GIT_TTL || (force && c.at < force);
      if (!stale || gitInflight.has(p.key) || !up(p.hostId)) continue;
      gitInflight.add(p.key);
      const done = (v: GitStatus | null) => {
        gitCache.set(p.key, { at: Date.now(), lastEnd: p.lastEnd, v });
        gitInflight.delete(p.key);
        setTick((n) => n + 1);
      };
      // Older daemons don't know git.status: show nothing for a while instead of asking again
      rpc<GitStatus | null>(p.hostId, 'git.status', { cwd: p.cwd }).then(done, () => done(null));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projects, inView, visible, force, hosts]);

  return (
    <section className="ov-panel" ref={ref}>
      <div className="ov-panel-h">
        {t('projects')}
        <span className="muted">{t('clickToFilter')}</span>
        <button className="ov-panel-lnk" onClick={() => setForce(Date.now())} title={t('refresh')}>
          <Icon name="refresh" size={13} />
        </button>
      </div>
      {projects.map((p) => {
        const g = gitCache.get(p.key);
        const sel = filter === p.key;
        return (
          <div
            key={p.key}
            className={`ov-proj${sel ? ' sel' : ''}`}
            role="button"
            tabIndex={0}
            onClick={() => setFilter(sel ? null : p.key)}
            onKeyDown={(e) => e.key === 'Enter' && e.target === e.currentTarget && setFilter(sel ? null : p.key)}
            title={p.cwd}
          >
            <div className="ov-proj-name">
              <span className="truncate">{basename(p.cwd) || p.cwd}</span>
              {multi && <span className="muted small">{p.hostName}</span>}
            </div>
            <div className="ov-proj-sub">
              {!g ? (
                <span className="ov-clean">{up(p.hostId) ? '…' : shortPath(p.cwd, p.home)}</span>
              ) : g.v ? (
                <>
                  <span className="ov-br">
                    {g.v.branch}
                    {g.v.ahead > 0 && ` ↑${g.v.ahead}`}
                    {g.v.behind > 0 && ` ↓${g.v.behind}`}
                  </span>
                  {g.v.changed > 0 ? <span>{t('uncommitted', { count: g.v.changed })}</span> : <span className="ov-clean">{t('noUncommitted')}</span>}
                </>
              ) : (
                <span className="ov-clean">{t('notAGitRepo')}</span>
              )}
            </div>
            <div className="ov-proj-tools">
              {p.tools.slice(0, 4).map((k) => (
                <ToolBadge key={k} tool={k} size={16} />
              ))}
            </div>
            <div className="ov-proj-acts" onClick={(e) => e.stopPropagation()}>
              {!!g?.v?.changed && (
                <button className="btn small" onClick={() => showChanges(p.hostId, p.latest.s.id)}>
                  {t('changes')}
                </button>
              )}
              {up(p.hostId) && (
                <button className="btn small" onClick={() => openNewSession({ hostId: p.hostId, cwd: p.cwd })}>
                  {t('newSession2')}
                </button>
              )}
            </div>
          </div>
        );
      })}
    </section>
  );
}

// ---------- Activity ----------

function feedText(f: FeedItem) {
  switch (f.kind) {
    case 'permission':
      return t('feedPermission', { v: f.text ?? '' });
    case 'question':
      return t('feedQuestion');
    case 'rejected':
      return t('rejected', { v: f.text ?? '' });
    case 'error':
      return t('feedError');
    case 'limit':
      return f.resumeAt ? t('feedLimitAt', { v: clock(f.resumeAt) }) : t('feedLimit');
    default:
      return f.files ? t('feedDoneFiles', { count: f.files }) : t('feedDone');
  }
}

const FEED_ICON: Record<FeedItem['kind'], [string, string]> = {
  permission: ['warn', 'shield'],
  question: ['warn', 'chat'],
  done: ['ok', 'check'],
  rejected: ['err', 'x'],
  error: ['err', 'x'],
  limit: ['info', 'clock'],
};

// `now` only re-renders the "x ago" texts
export function FeedPanel({ items }: { items: Item[]; now: number }) {
  const feed = useFeed();
  const byKey = useMemo(() => new Map(items.map((x) => [`${x.hostId}::${x.s.id}`, x])), [items]);
  const list = feed.filter((f) => byKey.has(`${f.hostId}::${f.sessionId}`)).slice(0, FEED_SHOWN);
  return (
    <section className="ov-panel">
      <div className="ov-panel-h">
        {t('activity')}
        <span className="muted">{t('sinceAppOpened')}</span>
      </div>
      {list.length === 0 ? (
        <div className="ov-feed-empty muted small">{t('feedEmpty')}</div>
      ) : (
        <ul className="ov-feed">
          {list.map((f) => {
            const x = byKey.get(`${f.hostId}::${f.sessionId}`)!;
            const [cls, icon] = FEED_ICON[f.kind];
            return (
              <li key={f.id} role="button" tabIndex={0} onClick={() => navigate({ kind: 'session', hostId: f.hostId, sessionId: f.sessionId })}>
                <span className={`ov-ic ${cls}`}>
                  <Icon name={icon} size={11} />
                </span>
                <span className="min0">
                  <b className="ov-feed-title">{x.s.title || t('newSession')}</b> <span className="ov-feed-s">{feedText(f)}</span>
                </span>
                <span className="ov-feed-w">{timeAgo(f.at)}</span>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}

// ---------- Last 7 days ----------

interface Stats {
  daily: { day: string; tool: string; usage: Usage }[];
}

const statsCache = new Map<string, { at: number; v: Stats | null }>();
const statsInflight = new Set<string>();
const tokensOf = (u: Usage) => u.inputTokens + u.outputTokens + u.cacheReadTokens + u.cacheWriteTokens;
const dayKey = (d: Date) => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;

export function WeekPanel({ hosts }: { hosts: HostEntry[] }) {
  const [, lang] = useLang();
  const ref = useRef<HTMLElement>(null);
  const inView = useInView(ref);
  const visible = usePageVisible();
  const [, setTick] = useState(0);
  const [force, setForce] = useState(0);
  const ids = hosts.map((h) => h.config.id);

  useEffect(() => {
    if (!inView || !visible) return;
    for (const id of ids) {
      const c = statsCache.get(id);
      if ((c && Date.now() - c.at < STATS_TTL && !(force && c.at < force)) || statsInflight.has(id)) continue;
      statsInflight.add(id);
      const done = (v: Stats | null) => {
        statsCache.set(id, { at: Date.now(), v });
        statsInflight.delete(id);
        setTick((n) => n + 1);
      };
      rpc<Stats>(id, 'stats.usage', { days: 7, tzOffset: -new Date().getTimezoneOffset() }).then(done, () => done(null));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ids.join(), inView, visible, force]);

  const loaded = ids.filter((id) => statsCache.get(id)?.v);
  const days: string[] = [];
  for (let i = 6; i >= 0; i--) days.push(dayKey(new Date(Date.now() - i * 86400000)));
  const recs = loaded.flatMap((id) => statsCache.get(id)!.v!.daily.filter((r) => days.includes(r.day)));
  // Cost only when every record has one: a mix (API-priced and subscription tools) would leave most bars empty
  const useCost = recs.some((r) => r.usage.costUsd > 0) && recs.every((r) => r.usage.costUsd > 0 || !tokensOf(r.usage));
  const val = (u: Usage) => (useCost ? u.costUsd : tokensOf(u));
  const by: Record<string, Record<string, number>> = {};
  const toolSet = new Set<string>();
  for (const r of recs) {
    (by[r.day] ??= {})[r.tool] = (by[r.day][r.tool] ?? 0) + val(r.usage);
    toolSet.add(r.tool);
  }
  const sum = (d: string) => Object.values(by[d] ?? {}).reduce((a, b) => a + b, 0);
  const max = Math.max(1e-9, ...days.map(sum));
  const total = days.reduce((a, d) => a + sum(d), 0);
  const tokensTotal = recs.reduce((a, r) => a + tokensOf(r.usage), 0);
  const fmt = (v: number) => (useCost ? `$${v.toFixed(2)}` : fmtTokens(v));
  const tools = [...toolSet];
  const locale = lang === 'en' ? 'en-US' : 'zh-CN';

  return (
    <section className="ov-panel" ref={ref}>
      <div className="ov-panel-h">
        {t('last7Days')}
        <button className="ov-panel-lnk" onClick={() => setForce(Date.now())} title={t('refresh')}>
          <Icon name="refresh" size={13} />
        </button>
        {hosts[0] && (
          <button className="ov-panel-lnk" onClick={() => navigate({ kind: 'stats', hostId: hosts[0].config.id })}>
            {t('stats')}
          </button>
        )}
      </div>
      <div className="ov-week">
        {loaded.length === 0 ? (
          <div className="muted small">{ids.every((id) => statsCache.has(id)) ? t('noUsage') : t('loading')}</div>
        ) : (
          <>
            <div className="ov-week-top">
              <span className="ov-big">{fmt(total)}</span>
              <span className="muted small">
                {useCost && `${fmtTokens(tokensTotal)} tokens · `}
                {t('today', { v: fmt(sum(days[6])) })}
              </span>
            </div>
            <div className="ov-week-bars">
              {days.map((d) => (
                <div key={d} className="ov-week-d" style={{ height: `${(sum(d) / max) * 100}%` }} title={`${d} · ${fmt(sum(d))}`}>
                  {tools.map((k) =>
                    by[d]?.[k] ? <i key={k} style={{ flexGrow: by[d][k], background: TOOL_VAR[k] ?? 'var(--text-3)' }} /> : null,
                  )}
                </div>
              ))}
            </div>
            <div className="ov-week-lbl">
              {days.map((d, i) => (
                <span key={d}>{i === 6 ? t('todayLabel') : new Date(`${d}T12:00:00`).toLocaleDateString(locale, { weekday: 'short' })}</span>
              ))}
            </div>
            <div className="ov-legend">
              {tools.map((k) => (
                <span key={k}>
                  <i style={{ background: TOOL_VAR[k] ?? 'var(--text-3)' }} />
                  {TOOL_LABEL[k as ToolKind] ?? k}
                </span>
              ))}
            </div>
          </>
        )}
      </div>
    </section>
  );
}

// ---------- Quota dialog ----------

export function QuotaDialog({ hosts, onClose }: { hosts: HostEntry[]; onClose: () => void }) {
  const [pick, setPick] = useState(hosts[0]?.config.id ?? '');
  const host = hosts.find((h) => h.config.id === pick) ?? hosts[0];
  useBack(true, onClose);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);
  const hasTools = QUOTA_TOOLS.some((k) => host?.status.hello?.tools.some((x) => x.kind === k && x.available));
  return (
    <div className="modal-bg" onClick={onClose}>
      <div className="modal ov-quota" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <h3>{t('toolQuota')}</h3>
          <button className="icon-btn" onClick={onClose} aria-label={t('close')}>
            <Icon name="x" />
          </button>
        </div>
        {hosts.length > 1 && (
          <div className="ov-seg">
            {hosts.map((h) => (
              <button key={h.config.id} className={h.config.id === host?.config.id ? 'on' : ''} onClick={() => setPick(h.config.id)}>
                {h.config.name}
              </button>
            ))}
          </div>
        )}
        {host && hasTools ? <QuotaCard key={host.config.id} hostId={host.config.id} /> : <div className="muted small">{t('noQuotaTools')}</div>}
      </div>
    </div>
  );
}
