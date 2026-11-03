import { getT } from '../i18n';
import { navigate, setState, useStore } from '../store';
import type { SessionInfo } from '../types';
import { fmtUsage, shortPath, STATUS_LABEL, timeAgo } from '../util';
import { Icon, ToolBadge } from './Icon';
import { sessionStatusClass } from './Sidebar';

const t = getT('home');

export function Home() {
  const hosts = useStore((s) => s.hosts);
  const sessions = useStore((s) => s.sessions);
  const all: { hostId: string; hostName: string; s: SessionInfo }[] = [];
  for (const h of hosts) for (const s of sessions[h.config.id] ?? []) if (!s.parentId) all.push({ hostId: h.config.id, hostName: h.config.name, s });
  const attn = all.filter((x) => x.s.pendingPermissions > 0);
  const active = all.filter((x) => ['running', 'starting'].includes(x.s.status) && x.s.pendingPermissions === 0);
  const recent = all
    .filter((x) => !attn.includes(x) && !active.includes(x))
    .sort((a, b) => b.s.updatedAt - a.s.updatedAt)
    .slice(0, 12);
  const connected = hosts.filter((h) => h.status.status === 'connected');

  const Row = ({ x }: { x: (typeof all)[number] }) => (
    <button className="list-row" onClick={() => navigate({ kind: 'session', hostId: x.hostId, sessionId: x.s.id })}>
      <ToolBadge tool={x.s.tool} size={22} />
      <div className="grow min0">
        <div className="truncate">{x.s.title || t('newSession')}</div>
        <div className="muted small truncate">
          {x.hostName} · {shortPath(x.s.cwd, hosts.find((h) => h.config.id === x.hostId)?.status.hello?.home)} · {timeAgo(x.s.updatedAt)}
        </div>
      </div>
      <span className="muted small hide-mobile">{fmtUsage(x.s.usage)}</span>
      {/* Files sessions have no process, so no status */}
      {x.s.tool !== 'files' && (
        <>
          <span className={`status-chip st-${x.s.status}`}>
            {x.s.pendingPermissions > 0 ? t('pending', { pendingPermissions: x.s.pendingPermissions }) : STATUS_LABEL[x.s.status]}
          </span>
          <span className={`sdot sdot-${sessionStatusClass(x.s)}`} />
        </>
      )}
    </button>
  );

  return (
    <div className="page">
      <div className="page-head">
        <h2>{t('overview')}</h2>
        {connected.length > 0 && (
          <button className="btn primary" onClick={() => setState({ newSession: { hostId: connected[0].config.id } })}>
            <Icon name="plus" size={14} /> {t('newSession2')}
          </button>
        )}
      </div>
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
      {attn.length > 0 && (
        <section>
          <h4 className="section-title attn-text">
            <Icon name="shield" size={14} /> {t('waitingForApproval')}
          </h4>
          <div className="list card-list">
            {attn.map((x) => (
              <Row key={x.s.id} x={x} />
            ))}
          </div>
        </section>
      )}
      {active.length > 0 && (
        <section>
          <h4 className="section-title">{t('running')}</h4>
          <div className="list card-list">
            {active.map((x) => (
              <Row key={x.s.id} x={x} />
            ))}
          </div>
        </section>
      )}
      {recent.length > 0 && (
        <section>
          <h4 className="section-title">{t('recent')}</h4>
          <div className="list card-list">
            {recent.map((x) => (
              <Row key={x.s.id} x={x} />
            ))}
          </div>
        </section>
      )}
    </div>
  );
}
