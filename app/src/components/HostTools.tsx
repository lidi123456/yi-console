import { useEffect, useState } from 'react';
import { getT, trErr, useLang } from '../i18n';
import { errorToast, setHostTools, setToolJob, useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type ToolInfo, type ToolJob, type ToolLatest } from '../types';
import { lsGet, lsSet } from '../util';
import { Icon, ToolBadge } from './Icon';

const tx = getT('hostTools');

/** First x.y.z in a `--version` line ("2.1.3 (Claude Code)", "codex-cli 0.160.1") */
export const semver = (v?: string | null) => v?.match(/\d+\.\d+\.\d+(?:[-+][\w.]+)?/)?.[0] ?? null;

/** a < b by numeric x.y.z; prerelease suffixes are ignored */
export function older(a: string, b: string) {
  const p = (s: string) => s.split(/[-+]/)[0].split('.').map(Number);
  const [x, y] = [p(a), p(b)];
  for (let i = 0; i < 3; i++) if ((x[i] ?? 0) !== (y[i] ?? 0)) return (x[i] ?? 0) < (y[i] ?? 0);
  return false;
}

const UPGRADABLE = ['claude', 'codex', 'cursor', 'kiro', 'qoder', 'codebuddy'];

/** Tools on one host in the host manager: version, newest version, and an upgrade with the tool's own update command */
export function HostTools({ hostId, tools }: { hostId: string; tools: ToolInfo[] }) {
  useLang();
  const jobs = useStore((s) => s.toolJobs[hostId]) ?? {};
  // undefined: loading; null: the daemon is too old for tools.latest
  const [latest, setLatest] = useState<Record<string, ToolLatest> | null | undefined>(undefined);
  const [checking, setChecking] = useState(false);
  const [open, setOpen] = useState<string | null>(null);
  // Collapsed by default, remembered per host on this device
  const [expanded, setExpanded] = useState(() => lsGet<boolean>(`host-tools-open:${hostId}`, false));
  const flip = () => {
    lsSet(`host-tools-open:${hostId}`, !expanded);
    setExpanded(!expanded);
  };

  const load = async (refresh: boolean) => {
    setChecking(true);
    try {
      const r = await rpc<{ tools: Record<string, ToolLatest>; jobs: Record<string, ToolJob>; info?: ToolInfo[] }>(hostId, 'tools.latest', {
        refresh,
      });
      setLatest(r.tools);
      // The hello the client cached when it connected predates any upgrade since
      if (r.info) setHostTools(hostId, r.info);
      for (const [k, j] of Object.entries(r.jobs ?? {})) setToolJob(hostId, k, j);
    } catch (e) {
      if (/未知方法|Unknown method/.test(String((e as Error).message ?? e))) setLatest(null);
      else errorToast(e);
    } finally {
      setChecking(false);
    }
  };
  useEffect(() => {
    load(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostId]);

  const update = async (kind: string) => {
    setOpen(kind);
    try {
      await rpc(hostId, 'tools.update', { tool: kind });
    } catch (e) {
      errorToast(e);
    }
  };
  // A finished upgrade changes the installed version: look up again so "update available" goes away
  const doneAt = Object.values(jobs)
    .map((j) => j.finishedAt ?? 0)
    .reduce((a, b) => Math.max(a, b), 0);
  useEffect(() => {
    if (doneAt) load(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [doneAt]);

  const anyRunning = Object.values(jobs).some((j) => j.running);
  const list = tools.filter((t) => UPGRADABLE.includes(t.kind));
  // An upgrade (also one started from another device) opens the list, and it stays open afterwards so the result shows;
  // this isn't remembered, only the user's own toggling is
  useEffect(() => {
    if (anyRunning) setExpanded(true);
  }, [anyRunning]);
  const show = expanded || anyRunning;
  const installed = list.filter((t) => t.available).length;
  const newerCount = list.filter((t) => {
    const l = latest?.[t.kind]?.latest;
    const c = semver(t.version);
    return t.available && l && c && older(c, l);
  }).length;
  return (
    <div className="host-tools">
      <div className="host-tools-head">
        <button className="host-tools-toggle" aria-expanded={show} onClick={flip}>
          <span className={show ? 'host-tools-chev open' : 'host-tools-chev'}>
            <Icon name="chevron" size={12} />
          </span>
          <span className="small" style={{ fontWeight: 600 }}>
            {tx('tools')}
          </span>
          <span className="muted small">
            {newerCount ? tx('summaryWithUpdates', { n: installed, m: newerCount }) : tx('summary', { n: installed })}
          </span>
        </button>
        {show &&
          (latest === null ? (
            <span className="muted small">{tx('needsNewerDaemon')}</span>
          ) : (
            <button className="btn small" disabled={checking} onClick={() => load(true)}>
              {checking ? tx('checking') : tx('checkForUpdates')}
            </button>
          ))}
      </div>
      {show && list.map((t) => {
        const job = jobs[t.kind];
        const info = latest?.[t.kind];
        const cur = semver(t.version);
        const newer = !!(info?.latest && cur && older(cur, info.latest));
        let status: React.ReactNode = null;
        if (!t.available) status = <span className="muted">{tx('notInstalled')}</span>;
        else if (job?.running) status = (
            <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
              <span className="spinner" /> {tx('updating')}
            </span>
          );
        else if (job?.ok === false) status = <span className="error-text">{job.error ? trErr(job.error) : tx('updateFailed')}</span>;
        else if (job?.ok && job.finishedAt && Date.now() - job.finishedAt < 10 * 60_000) {
          const [from, to] = [semver(job.from), semver(job.to)];
          status =
            from && to && from !== to ? (
              <span className="ok-text">{tx('updatedFromTo', { from, to })}</span>
            ) : (
              <span className="muted">{tx('updateFinished')}</span>
            );
        } else if (newer) status = <span className="attn-text">{tx('newVersion', { v: info!.latest! })}</span>;
        else if (info?.latest && cur) status = <span className="muted">{tx('upToDate')}</span>;
        else if (info?.error) status = <span className="muted" title={trErr(info.error)}>{tx('couldNotCheck')}</span>;
        const showLog = open === t.kind && !!job?.output?.length;
        return (
          <div key={t.kind} className="host-tool">
            <div className="host-tool-row">
              <ToolBadge tool={t.kind} size={18} />
              <div className="grow host-tool-main">
                <div>
                  <span style={{ fontWeight: 600 }}>{TOOL_LABEL[t.kind]}</span>
                  {t.available && <span className="muted small mono"> {cur ?? t.version ?? ''}</span>}
                  {info?.via === 'npm' && <span className="muted small">{` · ${info.via}`}</span>}
                </div>
                {t.path && (
                  <div className="muted small mono truncate" title={t.path}>
                    {t.path}
                  </div>
                )}
              </div>
              <div className="small host-tool-status">{status}</div>
              {!!job?.output?.length && (
                <button className="btn small" onClick={() => setOpen(open === t.kind ? null : t.kind)}>
                  {showLog ? tx('hideOutput') : tx('showOutput')}
                </button>
              )}
              {t.available && latest !== null && (
                <button
                  className={`btn small ${newer ? 'primary' : ''}`}
                  disabled={!info?.canUpdate || anyRunning}
                  title={tx('runsCommand', { cmd: `${t.kind === 'cursor' ? 'cursor-agent' : t.kind === 'kiro' ? 'kiro-cli' : t.kind === 'qoder' ? 'qodercli' : t.kind} update` })}
                  onClick={() => update(t.kind)}
                >
                  {tx('update')}
                </button>
              )}
            </div>
            {showLog && <pre className="host-tool-log">{job!.output.join('\n')}</pre>}
          </div>
        );
      })}
    </div>
  );
}
