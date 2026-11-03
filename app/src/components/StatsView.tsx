import { useEffect, useMemo, useState } from 'react';
import { getT, isEn } from '../i18n';
import { useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type Quota, type ToolKind, type Usage } from '../types';
import { fmtTokens } from '../util';
import { ToolBadge } from './Icon';

const tx = getT('statsView');

interface Stats {
  daily: { day: string; tool: string; usage: Usage }[];
  totals: Record<string, Usage>;
  /** Tokens per model for each tool (from the tool's own logs) */
  models?: Record<string, Record<string, number>>;
  /** Which tools' tokens come from local logs (older daemons don't send this field) */
  fromLogs?: string[];
}

export const errText = (e: unknown) => {
  const m = String((e as Error)?.message ?? e);
  return m.includes('未知方法') ? tx('thisHostSDaemonIs') : m;
};

const TOOL_VAR: Record<string, string> = {
  claude: 'var(--c-claude)',
  codex: 'var(--c-codex)',
  cursor: 'var(--c-cursor)',
  kiro: 'var(--c-kiro)',
  qoder: 'var(--c-qoder)',
  codebuddy: 'var(--c-codebuddy)',
  shell: 'var(--c-shell)',
};

const QUOTA_TOOLS: ToolKind[] = ['claude', 'codex', 'cursor', 'kiro', 'qoder', 'codebuddy'];

const fmtReset = (v: number | string | null | undefined) => {
  if (v == null) return '';
  const d = new Date(v);
  if (isNaN(d.getTime())) return '';
  const ms = d.getTime() - Date.now();
  if (ms <= 0) return tx('resetsSoon');
  const h = Math.floor(ms / 3600000);
  const mins = Math.ceil((ms % 3600000) / 60000);
  if (h < 24) return isEn() ? `resets in ${h > 0 ? `${h}h ` : ''}${mins}m` : `${h > 0 ? `${h} 小时` : ''}${mins} 分钟后重置`;
  const clock = `${d.getMonth() + 1}/${d.getDate()} ${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
  return isEn() ? `resets ${clock}` : `${clock} 重置`;
};

/** Quota windows for one tool: undefined while loading, a string is an error message */
export function QuotaBody({ q }: { q: Quota | string | undefined }) {
  return (
    <>
      {q === undefined && <div className="muted small">{tx('loading')}</div>}
      {typeof q === 'string' && <div className="muted small">{tx('failed')}{q}</div>}
      {typeof q === 'object' && (
        <>
          {q.windows.map((w) => (
            <div key={w.label} className="quota-win">
              <div className="row small">
                <span>{w.label}</span>
                <span className="grow" />
                <span className="muted">
                  {w.detail ?? (w.usedPercent != null ? tx('used', { v: Math.round(w.usedPercent) }) : '')}
                  {fmtReset(w.resetsAt) && ` · ${fmtReset(w.resetsAt)}`}
                </span>
              </div>
              {w.usedPercent != null && (
                <div className="bar">
                  <i
                    style={{ width: `${Math.min(100, Math.max(0, w.usedPercent))}%` }}
                    className={w.usedPercent >= 90 ? 'hot' : ''}
                  />
                </div>
              )}
            </div>
          ))}
          {q.note && <div className="muted small">{q.note}</div>}
        </>
      )}
    </>
  );
}

/** Account quotas of each tool. One request per tool, shown as each returns (the first time starts the tool briefly, a few seconds) */
function QuotaCard({ hostId }: { hostId: string }) {
  const hello = useStore((s) => s.hosts.find((h) => h.config.id === hostId)?.status.hello);
  const tools = useMemo(
    () => QUOTA_TOOLS.filter((k) => hello?.tools.some((t) => t.kind === k && t.available)),
    [hello],
  );
  const [res, setRes] = useState<Record<string, Quota | string | undefined>>({});

  const load = (refresh: boolean) => {
    setRes({});
    for (const tool of tools) {
      rpc<Quota>(hostId, 'tools.quota', { tool, refresh })
        .then((q) => setRes((r) => ({ ...r, [tool]: q })))
        .catch((e) => setRes((r) => ({ ...r, [tool]: errText(e) })));
    }
  };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(() => load(false), [hostId, tools.join()]);

  if (tools.length === 0) return null;
  return (
    <div className="card">
      <div className="card-title row">
        <span>{tx('accountQuota')}</span>
        <span className="grow" />
        <button className="btn small" onClick={() => load(true)}>
          {tx('refresh')}
        </button>
      </div>
      <div className="quota-list">
        {tools.map((t) => {
          const q = res[t];
          return (
            <div key={t} className="quota">
              <div className="row">
                <ToolBadge tool={t} size={18} /> <b>{TOOL_LABEL[t]}</b>
                {typeof q === 'object' && (q.account?.plan || q.account?.email) && (
                  <span className="muted small">{[q.account.plan, q.account.email].filter(Boolean).join(' · ')}</span>
                )}
              </div>
              <QuotaBody q={q} />
            </div>
          );
        })}
      </div>
    </div>
  );
}

const tokensOf = (u: Usage) => u.inputTokens + u.outputTokens + u.cacheReadTokens + u.cacheWriteTokens;

export function StatsView({ hostId }: { hostId: string }) {
  const [days, setDays] = useState(30);
  const [metric, setMetric] = useState<'tokens' | 'credits' | 'cost'>('tokens');
  const [data, setData] = useState<Stats | null>(null);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(true);
  const [hover, setHover] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setLoading(true);
    setError('');
    rpc<Stats>(hostId, 'stats.usage', { days, tzOffset: -new Date().getTimezoneOffset() })
      .then((d) => live && setData(d))
      .catch((e) => live && setError(errText(e)))
      .finally(() => live && setLoading(false));
    return () => {
      live = false;
    };
  }, [hostId, days]);

  const val = (u: Usage) => (metric === 'tokens' ? tokensOf(u) : metric === 'credits' ? u.credits : u.costUsd);

  const chart = useMemo(() => {
    const dayList: string[] = [];
    for (let i = days - 1; i >= 0; i--) {
      const d = new Date(Date.now() - i * 86400000);
      dayList.push(`${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`);
    }
    const by: Record<string, Record<string, number>> = {};
    for (const r of data?.daily ?? []) {
      (by[r.day] ??= {})[r.tool] = val(r.usage);
    }
    const max = Math.max(1e-9, ...dayList.map((d) => Object.values(by[d] ?? {}).reduce((a, b) => a + b, 0)));
    return { dayList, by, max };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [data, days, metric]);

  const tools = Object.keys(data?.totals ?? {}) as ToolKind[];
  const fmt = (v: number) => (metric === 'tokens' ? fmtTokens(v) : metric === 'credits' ? `${v.toFixed(2)} credits` : `$${v.toFixed(2)}`);
  const W = 640,
    H = 180,
    bw = W / chart.dayList.length;

  return (
    <div className="page">
      <div className="page-head">
        <h2>{tx('usage')}</h2>
        <div className="row">
          <div className="seg">
            {[7, 30, 90].map((d) => (
              <button key={d} className={days === d ? 'active' : ''} onClick={() => setDays(d)}>
                {tx('d', { d })}
              </button>
            ))}
          </div>
          <div className="seg">
            <button className={metric === 'tokens' ? 'active' : ''} onClick={() => setMetric('tokens')}>
              Tokens
            </button>
            <button className={metric === 'credits' ? 'active' : ''} onClick={() => setMetric('credits')}>
              {tx('credits')}
            </button>
            <button className={metric === 'cost' ? 'active' : ''} onClick={() => setMetric('cost')}>
              {tx('cost')}
            </button>
          </div>
        </div>
      </div>
      <QuotaCard hostId={hostId} />
      <p className="muted small">
        {data?.fromLogs
          ? tx('claudeCodeCodexQoderAnd')
          : tx('onlyStructuredYiConsoleSessions')}
      </p>
      {data && !data.fromLogs && (
        <div className="muted small">{tx('thisHostSDaemonIs2')}</div>
      )}
      <div className="stat-tiles">
        {loading && !data && <div className="muted">{tx('calculatingTheFirstReadOf')}</div>}
        {error && <div className="muted">{tx('failed')}{error}</div>}
        {!loading && !error && tools.length === 0 && <div className="muted">{tx('noUsageInThisRange')}</div>}
        {tools.map((t) => {
          const u = data!.totals[t];
          const tok = tokensOf(u);
          return (
            <div key={t} className="tile">
              <div className="row">
                <ToolBadge tool={t} size={18} /> {TOOL_LABEL[t] ?? t}
              </div>
              {tok > 0 || u.credits <= 0 ? (
                <>
                  <div className="tile-num">{fmtTokens(tok)}</div>
                  <div className="muted small">
                    {tx('in')} {fmtTokens(u.inputTokens)} · {tx('out')} {fmtTokens(u.outputTokens)} · {tx('cache')} {fmtTokens(u.cacheReadTokens)}
                  </div>
                  {u.costUsd > 0 && <div className="small">{tx('cost')} ${u.costUsd.toFixed(2)}</div>}
                  {u.credits > 0 && <div className="small">{u.credits.toFixed(2)} credits</div>}
                </>
              ) : (
                <>
                  <div className="tile-num">{u.credits.toFixed(2)} credits</div>
                  <div className="muted small">{tx('billedInCreditsNoTokens')}</div>
                </>
              )}
              {Object.entries(data!.models?.[t] ?? {})
                .sort((a, b) => b[1] - a[1])
                .slice(0, 3)
                .map(([m, n]) => (
                  <div key={m} className="muted small truncate" title={m}>
                    {m} · {tok > 0 ? fmtTokens(n) : `${n.toFixed(2)} credits`}
                  </div>
                ))}
            </div>
          );
        })}
      </div>
      {tools.length > 0 && (
        <div className="card chart-card">
          <svg viewBox={`0 0 ${W} ${H + 22}`} className="chart" role="img" aria-label={tx('dailyUsage')}>
            {[0.25, 0.5, 0.75, 1].map((f) => (
              <line key={f} x1={0} x2={W} y1={H - H * f} y2={H - H * f} className="grid" />
            ))}
            {chart.dayList.map((d, i) => {
              let y = H;
              const segs = tools.map((t) => {
                const v = chart.by[d]?.[t] ?? 0;
                const h = (v / chart.max) * (H - 6);
                y -= h;
                return v > 0 ? (
                  <rect key={t} x={i * bw + bw * 0.15} y={y} width={bw * 0.7} height={Math.max(h - 1, 0.5)} fill={TOOL_VAR[t]} rx={1.5} />
                ) : null;
              });
              return (
                <g key={d} onMouseEnter={() => setHover(d)} onMouseLeave={() => setHover(null)}>
                  <rect x={i * bw} y={0} width={bw} height={H} fill="transparent" />
                  {segs}
                  {(chart.dayList.length - 1 - i) % Math.ceil(chart.dayList.length / 8) === 0 && (
                    <text
                      x={i === 0 ? 0 : i === chart.dayList.length - 1 ? (i + 1) * bw : i * bw + bw / 2}
                      y={H + 15}
                      className="axis"
                      textAnchor={i === 0 ? 'start' : i === chart.dayList.length - 1 ? 'end' : 'middle'}
                    >
                      {d.slice(5)}
                    </text>
                  )}
                </g>
              );
            })}
          </svg>
          <div className="chart-foot small">
            {hover ? (
              <>
                <b>{hover}</b>{' '}
                {tools.map((t) => (
                  <span key={t} className="legend">
                    <i style={{ background: TOOL_VAR[t] }} /> {TOOL_LABEL[t] ?? t} {fmt(chart.by[hover]?.[t] ?? 0)}
                  </span>
                ))}
              </>
            ) : (
              tools.map((t) => (
                <span key={t} className="legend">
                  <i style={{ background: TOOL_VAR[t] }} /> {TOOL_LABEL[t] ?? t}
                </span>
              ))
            )}
          </div>
        </div>
      )}
    </div>
  );
}
