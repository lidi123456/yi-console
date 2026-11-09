import { memo, useMemo, useState } from 'react';
import { getT, useLang } from '../i18n';
import { markSeen, projKey, relPath, unseenTurn, useNow, useSeenVersion } from '../overview';
import { permLabel } from '../perm';
import { errorToast, navigate, openNewSession, useStore } from '../store';
import { rpc } from '../transport';
import type { HostEntry } from '../store';
import type { SessionInfo } from '../types';
import { basename, fmtTokens, shortPath, timeAgo } from '../util';
import { Icon, ToolBadge } from './Icon';
import { FeedPanel, ProjectPanel, QuickStart, QuotaDialog, WeekPanel } from './OverviewPanels';
import { showChanges } from './SessionActions';

const t = getT('home');

/** One session on the board */
export interface Item {
  hostId: string;
  hostName: string;
  home?: string;
  s: SessionInfo;
}

type Col = 'attn' | 'run' | 'review' | 'rest';
const COLS: Col[] = ['attn', 'run', 'review', 'rest'];
/** Compact rows shown in the last column before "Show all" */
const REST_SHOWN = 6;
/** An error older than this no longer asks for attention (it stays in the last column) */
const ERROR_FRESH_MS = 24 * 3600 * 1000;

function colOf(x: Item): Col {
  const s = x.s;
  if (s.tool === 'files') return 'rest';
  if (s.pendingPermissions > 0 || s.status === 'waiting-permission' || s.restartInterrupted) return 'attn';
  if (s.status === 'error' && Date.now() - s.updatedAt < ERROR_FRESH_MS) return 'attn';
  // Terminal sessions stay "running" as long as their process lives, so they don't get a column of their own
  if (s.mode !== 'structured') return 'rest';
  if (s.status === 'running' || s.status === 'starting' || s.autoResume) return 'run';
  if (unseenTurn(x.hostId, s)) return 'review';
  return 'rest';
}

const open = (x: Item) => navigate({ kind: 'session', hostId: x.hostId, sessionId: x.s.id });


function minutes(ms: number) {
  return Math.floor(ms / 60000);
}

/** What kind of tool call an activity is, from the tool name the adapter reports */
function actKind(name: string) {
  const n = name.toLowerCase();
  if (n.includes('todo')) return 'plan';
  if (/web|fetch|http|browser|url/.test(n)) return 'web';
  if (/bash|shell|exec|command|terminal/.test(n)) return 'command';
  if (/edit|write|patch|notebook|delete|move|create/.test(n)) return 'edit';
  if (/read|view|cat/.test(n)) return 'read';
  if (/grep|glob|search|find|list|ls/.test(n)) return 'search';
  if (/task|agent/.test(n)) return 'agent';
  return 'other';
}

function actLabel(name: string) {
  switch (actKind(name)) {
    case 'plan':
      return t('actPlan');
    case 'web':
      return t('actWeb');
    case 'command':
      return t('actCommand');
    case 'edit':
      return t('actEdit');
    case 'read':
      return t('actRead');
    case 'search':
      return t('actSearch');
    case 'agent':
      return t('actAgent');
    default:
      return name;
  }
}

function colTitle(c: Col) {
  switch (c) {
    case 'attn':
      return t('colAttn');
    case 'run':
      return t('colRun');
    case 'review':
      return t('colReview');
    default:
      return t('colRest');
  }
}

const COL_DOT: Record<Col, string> = { attn: 'attn', run: 'busy', review: 'ok', rest: 'idle' };

async function answer(x: Item, requestId: string, optionId: string) {
  try {
    await rpc(x.hostId, 'session.permission', { sessionId: x.s.id, requestId, optionId });
  } catch (e) {
    errorToast(e);
  }
}

async function interrupted(x: Item, action: 'continue' | 'dismiss') {
  try {
    await rpc(x.hostId, 'session.interrupted', { sessionId: x.s.id, action });
  } catch (e) {
    errorToast(e);
  }
}

/** Card shell: the whole card opens the session; buttons inside stop the click */
function Card({ x, edge, children, multi }: { x: Item; edge: string; children: React.ReactNode; multi: boolean }) {
  const s = x.s;
  return (
    <div
      className={`ov-card ov-e-${edge}`}
      role="button"
      tabIndex={0}
      onClick={() => open(x)}
      onKeyDown={(e) => e.key === 'Enter' && e.target === e.currentTarget && open(x)}
    >
      <div className="ov-card-top">
        <ToolBadge tool={s.tool} size={22} />
        <div className="min0 grow">
          <div className="ov-card-title">{s.title || t('newSession')}</div>
          <div className="ov-meta">
            {multi && (
              <>
                <span className="truncate">{x.hostName}</span>
                <span className="ov-sep">·</span>
              </>
            )}
            <span className="truncate" title={s.cwd}>
              {shortPath(s.cwd, x.home)}
            </span>
          </div>
        </div>
        {edge === 'ok' && <span className="ov-unread" />}
      </div>
      {children}
    </div>
  );
}

const stop = (e: React.SyntheticEvent) => e.stopPropagation();

const AttnCard = memo(function AttnCard({ x, multi }: { x: Item; multi: boolean }) {
  const s = x.s;
  const p = s.pending;
  const err = s.status === 'error' && !s.pendingPermissions && !s.restartInterrupted;
  const allow = p?.options?.find((o) => o.kind === 'allow_once') ?? p?.options?.find((o) => o.kind.startsWith('allow'));
  const reject = p?.options?.find((o) => o.kind === 'reject_once') ?? p?.options?.find((o) => o.kind.startsWith('reject'));
  // Answer from the card only when this is the only request and a plain approval
  const inline = p && !p.question && s.pendingPermissions === 1 && allow && reject ? { p, allow, reject } : null;
  const more = Math.max(0, s.pendingPermissions - 1);
  return (
    <Card x={x} edge={err ? 'err' : 'attn'} multi={multi}>
      {s.pendingPermissions > 0 && (
        <div className="ov-ask">
          <span className="muted small">
            {p?.question ? t('asksAQuestion') : p?.detailKind === 'command' ? t('wantsToRun') : p?.detailKind === 'file' ? t('wantsToEdit') : t('wantsApproval')}
            {p && ` · ${timeAgo(p.at)}`}
            {more > 0 && ` · ${t('morePending', { n: more })}`}
          </span>
          {p && (
            <code className={p.question ? 'ov-ask-q' : ''}>
              {p.detailKind === 'file' && p.detail ? relPath(p.detail, s.cwd) : p.detail || p.title}
            </code>
          )}
        </div>
      )}
      {s.restartInterrupted && !s.pendingPermissions && <div className="ov-tags"><span className="ov-chip warn">{t('cutOffByRestart')}</span></div>}
      {err && <div className="ov-tags"><span className="ov-chip err">{t('exitedWithError')}</span></div>}
      <div className="ov-acts" onClick={stop}>
        {inline ? (
          <>
            <button className="btn small" onClick={() => answer(x, inline.p.requestId, inline.reject.id)}>
              {t('reject')}
            </button>
            <button className="btn small" onClick={() => open(x)}>
              {t('open')}
            </button>
            <button className="btn small primary" onClick={() => answer(x, inline.p.requestId, inline.allow.id)}>
              {t('allow')}
            </button>
          </>
        ) : s.pendingPermissions > 0 ? (
          <button className="btn small primary" onClick={() => open(x)}>
            {p?.question ? t('answer') : t('openApprovals')}
          </button>
        ) : s.restartInterrupted ? (
          <>
            <button className="btn small" onClick={() => interrupted(x, 'dismiss')}>
              {t('dismiss')}
            </button>
            <button className="btn small primary" onClick={() => interrupted(x, 'continue')}>
              {t('continueTurn')}
            </button>
          </>
        ) : (
          <button className="btn small primary" onClick={() => open(x)}>
            {t('open')}
          </button>
        )}
      </div>
    </Card>
  );
});

/** mm:ss until `at`, ticking every second while visible */
function Countdown({ at }: { at: number }) {
  const now = useNow(1000);
  const left = Math.max(0, Math.round((at - now) / 1000));
  const h = Math.floor(left / 3600);
  const m = Math.floor((left % 3600) / 60);
  const sec = String(left % 60).padStart(2, '0');
  return <span className="ov-timer">{h > 0 ? `${h}:${String(m).padStart(2, '0')}:${sec}` : `${m}:${sec}`}</span>;
}

const RunCard = memo(function RunCard({ x, multi, now }: { x: Item; multi: boolean; now: number }) {
  const s = x.s;
  const turn = s.turn && !s.turn.endedAt ? s.turn : undefined;
  const a = turn?.activity;
  const files = turn?.filesChanged ?? 0;
  const cost = s.usage.costUsd ? `$${s.usage.costUsd.toFixed(2)}` : '';
  const tokens = s.usage.inputTokens + s.usage.outputTokens + s.usage.cacheReadTokens + s.usage.cacheWriteTokens;
  const bg = s.backgroundTasks?.length ?? 0;
  const queued = s.queued?.length ?? 0;
  return (
    <Card x={x} edge={s.autoResume ? 'attn' : 'busy'} multi={multi}>
      {a && (
        <div className="ov-act">
          <span className="ov-act-k">{actLabel(a.name)}</span>
          <code title={a.title}>{actKind(a.name) === 'edit' || actKind(a.name) === 'read' ? relPath(a.title, s.cwd) : a.title}</code>
        </div>
      )}
      <div className="ov-tags">
        {s.autoResume && <span className="ov-chip warn">{t('usageLimitReached')}</span>}
        {s.model && <span className="ov-chip">{s.effort ? `${s.model} · ${s.effort}` : s.model}</span>}
        {s.permissionMode && s.permissionMode !== 'default' && <span className="ov-chip">{permLabel(s.permissionMode)}</span>}
        {bg > 0 && <span className="ov-chip busy">{t('backgroundTasks', { n: bg })}</span>}
        {queued > 0 && <span className="ov-chip">{t('queued', { n: queued })}</span>}
      </div>
      {s.contextPercent != null && !s.autoResume && (
        <div className="ov-ctx">
          <span>{t('context')}</span>
          <div className="ov-bar">
            <i style={{ width: `${Math.min(100, Math.max(0, s.contextPercent))}%` }} />
          </div>
          <span>{Math.round(s.contextPercent)}%</span>
        </div>
      )}
      <div className="ov-foot">
        {s.autoResume ? (
          <>
            <span>{t('continuesIn')}</span>
            <span className="grow" />
            <Countdown at={s.autoResume.at} />
          </>
        ) : (
          <>
            <span>
              {turn ? (minutes(now - turn.startedAt) ? t('runningFor', { n: minutes(now - turn.startedAt) }) : t('justStarted')) : s.status === 'starting' ? t('starting') : ''}
              {turn && ` · ${files ? t('filesChanged', { count: files }) : t('noFilesChanged')}`}
            </span>
            <span className="grow" />
            <span>{cost || (tokens ? `${fmtTokens(tokens)} tok` : '')}</span>
          </>
        )}
      </div>
    </Card>
  );
});

const ReviewCard = memo(function ReviewCard({ x, multi }: { x: Item; multi: boolean }) {
  const s = x.s;
  const turn = s.turn!;
  const files = turn.filesChanged ?? 0;
  const read = () => markSeen([{ hostId: x.hostId, sessionId: s.id }]);
  return (
    <Card x={x} edge="ok" multi={multi}>
      {turn.rejected && (
        <div className="ov-tags">
          <span className="ov-chip err" title={turn.rejected}>
            {t('rejected', { v: relPath(turn.rejected, s.cwd) })}
          </span>
        </div>
      )}
      <div className="ov-foot">
        <span>
          {t('finishedAgo', { v: timeAgo(turn.endedAt!) })}
          {` · ${minutes(turn.endedAt! - turn.startedAt) ? t('took', { n: minutes(turn.endedAt! - turn.startedAt) }) : t('tookUnderAMinute')}`}
          {` · ${files ? t('filesChanged', { count: files }) : t('noFilesChanged')}`}
        </span>
      </div>
      <div className="ov-acts" onClick={stop}>
        <button className="btn small" onClick={read}>
          {t('markRead')}
        </button>
        {files > 0 ? (
          <button className="btn small primary" onClick={() => showChanges(x.hostId, s.id)}>
            {t('viewChanges')}
          </button>
        ) : (
          <button className="btn small primary" onClick={() => open(x)}>
            {t('open')}
          </button>
        )}
      </div>
    </Card>
  );
});

const MiniRow = memo(function MiniRow({ x }: { x: Item }) {
  const s = x.s;
  const live = s.tool !== 'files' && (s.status === 'running' || s.status === 'starting');
  return (
    <button className="ov-mini" onClick={() => open(x)} title={`${s.title || t('newSession')}\n${s.cwd}`}>
      <ToolBadge tool={s.tool} size={16} />
      <span className="ov-mini-t truncate">{s.title || `${t('newSession')} · ${basename(s.cwd)}`}</span>
      {live && <span className="sdot sdot-busy" />}
      <span className="ov-mini-w">{timeAgo(s.updatedAt)}</span>
    </button>
  );
});

function Column({
  col,
  items,
  multi,
  now,
  shown,
}: {
  col: Col;
  items: Item[];
  multi: boolean;
  now: number;
  shown: boolean;
}) {
  const [all, setAll] = useState(false);
  const rest = col === 'rest';
  const list = rest && !all ? items.slice(0, REST_SHOWN) : items;
  return (
    <section className={`ov-col ov-col-${col}${shown ? ' ov-show' : ''}`}>
      <div className="ov-col-h">
        <span className={`sdot sdot-${COL_DOT[col]}`} />
        {colTitle(col)}
        <span className="ov-n">{items.length}</span>
        {col === 'review' && items.length > 0 && (
          <button className="ov-col-act" onClick={() => markSeen(items.map((x) => ({ hostId: x.hostId, sessionId: x.s.id })))}>
            {t('markAllRead')}
          </button>
        )}
      </div>
      {items.length === 0 && (
        <div className="ov-empty">
          {col === 'attn' ? t('emptyAttn') : col === 'run' ? t('emptyRun') : col === 'review' ? t('emptyReview') : t('emptyRest')}
        </div>
      )}
      {list.map((x) =>
        col === 'attn' ? (
          <AttnCard key={x.hostId + x.s.id} x={x} multi={multi} />
        ) : col === 'run' ? (
          <RunCard key={x.hostId + x.s.id} x={x} multi={multi} now={now} />
        ) : col === 'review' ? (
          <ReviewCard key={x.hostId + x.s.id} x={x} multi={multi} />
        ) : (
          <MiniRow key={x.hostId + x.s.id} x={x} />
        ),
      )}
      {rest && items.length > REST_SHOWN && (
        <button className="ov-more" onClick={() => setAll(!all)}>
          {all ? t('showLess') : t('showAll', { n: items.length })}
        </button>
      )}
    </section>
  );
}

function HostLine({ hosts, counts }: { hosts: HostEntry[]; counts: Record<string, number> }) {
  return (
    <div className="ov-hosts">
      {hosts.map((h) => {
        const st = h.status.status;
        return (
          <button key={h.config.id} className="ov-hchip" onClick={() => navigate({ kind: 'hosts' })}>
            <span className={`sdot sdot-${st === 'connected' ? 'ok' : st === 'error' ? 'err' : st === 'connecting' ? 'busy' : 'idle'}`} />
            <b>{h.config.name}</b>
            {st === 'connected'
              ? counts[h.config.id] ?? 0
              : st === 'connecting'
                ? t('connecting')
                : st === 'error'
                  ? t('connectionFailed')
                  : t('disconnected')}
          </button>
        );
      })}
      <button className="ov-hchip ov-hchip-add" onClick={() => navigate({ kind: 'hosts' })}>
        <Icon name="plus" size={12} /> {t('host')}
      </button>
    </div>
  );
}

export function Home() {
  const [, lang] = useLang();
  const hosts = useStore((s) => s.hosts);
  const sessions = useStore((s) => s.sessions);
  const seenV = useSeenVersion();
  const [filter, setFilter] = useState<string | null>(null);
  const [quota, setQuota] = useState(false);
  const [tab, setTab] = useState<Col | null>(null);

  const all = useMemo(() => {
    const out: Item[] = [];
    for (const h of hosts) {
      const home = h.status.hello?.home;
      for (const s of sessions[h.config.id] ?? []) if (!s.parentId) out.push({ hostId: h.config.id, hostName: h.config.name, home, s });
    }
    return out;
  }, [hosts, sessions]);

  const counts = useMemo(() => {
    const c: Record<string, number> = {};
    for (const x of all) c[x.hostId] = (c[x.hostId] ?? 0) + 1;
    return c;
  }, [all]);

  const cols = useMemo(() => {
    const by: Record<Col, Item[]> = { attn: [], run: [], review: [], rest: [] };
    for (const x of all) {
      if (filter && projKey(x.hostId, x.s.cwd) !== filter) continue;
      by[colOf(x)].push(x);
    }
    // Oldest request first: it has waited longest. The rest newest first (the list arrives sorted that way)
    by.attn.sort((a, b) => (a.s.pending?.at ?? a.s.updatedAt) - (b.s.pending?.at ?? b.s.updatedAt));
    by.review.sort((a, b) => (b.s.turn?.endedAt ?? 0) - (a.s.turn?.endedAt ?? 0));
    for (const c of COLS) if (c !== 'attn' && c !== 'review') by[c].sort((a, b) => b.s.updatedAt - a.s.updatedAt);
    return by;
    // seenV: unseenTurn reads the seen marks
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [all, filter, seenV]);

  // Elapsed minutes on running cards and "x ago" texts: every 30 s while something runs, else every 5 minutes
  const now = useNow(cols.run.length ? 30_000 : 300_000);
  const connected = hosts.filter((h) => h.status.status === 'connected');
  const multi = hosts.length > 1;
  const limited = all.some((x) => x.s.autoResume || (x.s.limitHits ?? 0) > 0);
  const shownTab = tab ?? COLS.find((c) => cols[c].length > 0) ?? 'attn';
  const filterName = useMemo(() => {
    if (!filter) return '';
    const x = all.find((i) => projKey(i.hostId, i.s.cwd) === filter);
    return x ? (multi ? `${basename(x.s.cwd)} · ${x.hostName}` : basename(x.s.cwd)) : '';
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filter, all, multi, lang]);

  return (
    <div className="page ov-page">
      <div className="ov-head">
        <h2>{t('overview')}</h2>
        {(cols.run.length > 0 || cols.attn.length > 0) && (
          <span className="ov-sub">
            {cols.run.length > 0 && t('runningCount', { n: cols.run.length })}
            {cols.run.length > 0 && cols.attn.length > 0 && ' · '}
            {cols.attn.length > 0 && <b>{t('attnCount', { n: cols.attn.length })}</b>}
          </span>
        )}
        <span className="grow" />
        {connected.length > 0 && (
          <button className="btn" onClick={() => setQuota(true)} title={limited ? t('aToolHitItsLimit') : undefined}>
            {limited && <span className="ov-warn-dot" />}
            {t('quota')}
          </button>
        )}
        {connected.length > 0 && (
          <button className="btn primary" onClick={() => openNewSession({ hostId: connected[0].config.id })}>
            <Icon name="plus" size={14} /> {t('newSession2')}
          </button>
        )}
      </div>
      {hosts.length > 0 && <HostLine hosts={hosts} counts={counts} />}
      {hosts.length === 0 && <div className="muted">{t('loading')}</div>}
      {hosts.length > 0 && connected.length === 0 && (
        <div className="card">
          <div className="card-title">{t('noHostsConnected')}</div>
          <p className="muted small">{t('addAnSshHostOr')}</p>
          <button className="btn primary" onClick={() => navigate({ kind: 'hosts' })}>
            {t('manageHosts')}
          </button>
        </div>
      )}
      {hosts.length > 0 && <QuickStart hosts={hosts} sessions={sessions} />}

      <div className="ov-tabs">
        {COLS.map((c) => (
          <button key={c} className={`${c === shownTab ? 'on' : ''}${c === 'attn' && cols.attn.length ? ' attn' : ''}`} onClick={() => setTab(c)}>
            {c === 'rest' ? t('tabRest') : colTitle(c)}
            <span className="ov-n">{cols[c].length}</span>
          </button>
        ))}
      </div>
      {filter && (
        <div className="ov-filterbar">
          <span>{t('onlyProject', { name: filterName })}</span>
          <button onClick={() => setFilter(null)}>
            {t('showEverything')} <Icon name="x" size={12} />
          </button>
        </div>
      )}
      <div className="ov-board">
        {COLS.map((c) => (
          <Column key={c} col={c} items={cols[c]} multi={multi} now={now} shown={c === shownTab} />
        ))}
      </div>

      {all.length > 0 && (
        <div className="ov-lower">
          <ProjectPanel items={all} filter={filter} setFilter={setFilter} multi={multi} />
          <div className="ov-lower-r">
            <FeedPanel items={all} now={now} />
            <WeekPanel hosts={connected} />
          </div>
        </div>
      )}
      {quota && <QuotaDialog hosts={connected} onClose={() => setQuota(false)} />}
    </div>
  );
}
