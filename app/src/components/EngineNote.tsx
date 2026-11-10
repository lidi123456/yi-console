// What the desktop app's Bot engine sends to a Bot or task session (`UserMessage.origin === 'bot-engine'`): event
// cards, task reports, routine starts, approval outcomes, notices. The AI reads the tagged text; the chat shows each
// block as a one-line note, with the original text behind a toggle.

import { useMemo, useState } from 'react';
import { getT, useLang } from '../i18n';
import { Icon } from './Icon';

const t = getT('bots');

export const ENGINE_ORIGIN = 'bot-engine';

interface Block {
  tag: string;
  attrs: Record<string, string>;
  body: string;
}

const BLOCK = /<([a-z][a-z-]*)((?:\s+[a-z_-]+="[^"]*")*)\s*>([\s\S]*?)<\/\1>/g;
const ATTR = /([a-z_-]+)="([^"]*)"/g;

/** The tagged blocks of an engine message; text outside them (instructions for the AI) is left out */
export function parseEngine(text: string): Block[] {
  const out: Block[] = [];
  for (const m of text.matchAll(BLOCK)) {
    const attrs: Record<string, string> = {};
    for (const a of m[2].matchAll(ATTR)) attrs[a[1]] = a[2];
    out.push({ tag: m[1], attrs, body: m[3].trim() });
  }
  return out;
}

const firstLine = (s: string) => s.split('\n').map((l) => l.trim()).find((l) => l) ?? '';

const REPORT_STATES: Record<string, 'noteReportReady' | 'noteReportProgress' | 'noteReportBlocked' | 'noteReportFailed' | 'noteReportDropped' | 'noteReportWaiting'> = {
  ready: 'noteReportReady',
  progress: 'noteReportProgress',
  blocked: 'noteReportBlocked',
  failed: 'noteReportFailed',
  dropped: 'noteReportDropped',
  waiting: 'noteReportWaiting',
};

/** Icon, one-line title and an optional detail line for a block */
function describe(b: Block): { icon: string; title: string; detail?: string; tone?: string } {
  const a = b.attrs;
  switch (b.tag) {
    case 'event': {
      // The card's text, after the header, may end with the event's link
      const sender = (a.sender ?? '').replace(/\s*\([^)]*\)$/, '');
      return {
        icon: 'bell',
        title: a.kind === 'ambient' ? t('noteEventAmbient', { source: a.source ?? '' }) : t('noteEvent', { source: a.source ?? '' }),
        detail: [sender, firstLine(b.body)].filter(Boolean).join(t('noteSep')),
      };
    }
    case 'report': {
      if (a.auto === 'true') {
        const rest = b.body.split('\n').slice(1).join('\n');
        return { icon: 'bot', title: t('noteReportAuto', { title: a.title ?? '' }), detail: firstLine(rest) || undefined, tone: 'warn' };
      }
      const key = REPORT_STATES[a.state ?? ''] ?? 'noteReportProgress';
      const tone = a.state === 'failed' || a.state === 'blocked' ? 'warn' : a.state === 'ready' ? 'ok' : undefined;
      return { icon: 'bot', title: t('noteReport', { title: a.title ?? '', state: t(key) }), detail: firstLine(b.body), tone };
    }
    case 'routine':
      return { icon: 'clock', title: a.manual === 'true' ? t('noteRoutineManual', { name: a.name ?? '' }) : t('noteRoutine', { name: a.name ?? '' }) };
    case 'approval': {
      const tool = a.kind === 'tool';
      const key =
        a.state === 'approved' ? (tool ? 'noteApprovedTool' : 'noteApprovedReply') : a.state === 'rejected' ? (tool ? 'noteRejectedTool' : 'noteRejectedReply') : tool ? 'noteExpiredTool' : 'noteExpiredReply';
      return { icon: 'shield', title: t(key), detail: a.text, tone: a.state === 'approved' ? 'ok' : undefined };
    }
    case 'owner':
      switch (a.event) {
        case 'change-approved':
          return { icon: 'settings', title: t('noteChangeApproved'), detail: a.summary, tone: 'ok' };
        case 'change-declined':
          return { icon: 'settings', title: t('noteChangeDeclined'), detail: a.summary };
        case 'secret-filled':
          return { icon: 'key', title: t('noteSecretFilled', { name: a.name ?? '' }), tone: 'ok' };
        case 'secret-dismissed':
          return { icon: 'key', title: t('noteSecretDismissed') };
        default:
          return { icon: 'bot', title: t('noteOwner') };
      }
    case 'handback':
      return a.event
        ? { icon: 'back', title: t('noteHandbackEvent'), detail: firstLine(b.body) }
        : { icon: 'back', title: t('noteHandback') };
    case 'follow-up':
      return { icon: 'forward', title: t('noteFollowUp') };
    case 'notes':
      return { icon: 'file', title: t('noteNotes') };
    case 'bot-message':
      return { icon: 'bot', title: t('noteBotMessage'), detail: b.body };
    default:
      return { icon: 'bot', title: b.tag };
  }
}

/** An engine message in the chat: a note per block, the original text on demand */
export function EngineNote({ text }: { text: string }) {
  const [lang] = useLang();
  const [raw, setRaw] = useState(false);
  const rows = useMemo(() => parseEngine(text).map(describe), [text, lang]);
  return (
    <div className="engine-note">
      {/* Says outright that no person typed this, even though it sits where a sent message would */}
      <div className="engine-note-head">
        <span className="engine-note-tag">
          <Icon name="bot" size={12} />
          {t('noteSentBy')}
        </span>
        <button className="link-btn small engine-note-raw" onClick={() => setRaw(!raw)}>
          {raw ? t('noteHideRaw') : t('noteShowRaw')}
        </button>
      </div>
      {rows.length === 0 && (
        <div className="engine-note-row">
          <Icon name="bot" size={13} />
          <span className="engine-note-title">{t('noteOwner')}</span>
        </div>
      )}
      {rows.map((r, i) => (
        <div key={i} className={`engine-note-row ${r.tone ?? ''}`}>
          <Icon name={r.icon as any} size={13} />
          <span className="engine-note-title">{r.title}</span>
          {r.detail && <span className="engine-note-detail">{r.detail}</span>}
        </div>
      ))}
      {raw && <pre className="engine-note-pre">{text}</pre>}
    </div>
  );
}
